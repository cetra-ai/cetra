//! WAL record framing.
//!
//! Spec 03 §1. Integers are little-endian. The checksum covers `seq` as 8
//! little-endian bytes, then `kind` as one byte, then the payload.
//!
//! Called by the segment writer once it exists. The tests in this file cover it until then.
#![allow(dead_code)]

use std::io::Read;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};

use arrow::array::RecordBatch;
use arrow::datatypes::Schema;
use arrow::error::ArrowError;
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::{IpcWriteOptions, StreamWriter};
use arrow::ipc::{BodyCompressionMethod, CompressionType, Message};

use crate::error::WalError;
use crate::wal::Table;

/// `payload_len` + checksum + `seq` + `kind`.
pub(crate) const HEADER_LEN: usize = 17;

/// Reject larger records on write, and treat them as invalid on read.
///
/// A corrupt length must not turn into a multi-gigabyte allocation.
pub(crate) const MAX_PAYLOAD: u32 = 256 * 1024 * 1024;

const KIND_BATCH: u8 = 1;

/// Most decompressed bytes one record may declare across its IPC buffers.
///
/// The writer rejects a batch over this, so replay never calls acknowledged data
/// corrupt. A batch is bounded by the memtable's `hard_limit`, which has no upper
/// bound of its own, not by `ingest.max_request_bytes`, so the cap has to be
/// enforced on write. Replay treats a larger declared size as corrupt.
const MAX_DECODED_BYTES: u64 = 1 << 30;

/// LZ4 expands a block by at most about 255 to 1. The slack covers the frame header.
const LZ4_MAX_RATIO: u64 = 256;
const LZ4_SLACK: u64 = 64;

/// One step of [`decode_next`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decoded {
    /// A framed record. `payload` indexes the IPC bytes inside the buffer.
    Record {
        /// WAL sequence number.
        seq: u64,
        /// Byte range of the IPC payload.
        payload: Range<usize>,
        /// Offset of the next record.
        next: usize,
    },
    /// `offset` is the end of the buffer.
    End,
    /// The header or the payload runs past the buffer. A torn tail.
    Short {
        /// Offset where the incomplete record starts.
        offset: usize,
    },
    /// Checksum, kind, or length is wrong. Not a torn tail by itself.
    Invalid {
        /// Offset where the bad record starts.
        offset: usize,
        /// Static reason. Never payload bytes.
        reason: &'static str,
    },
}

/// Appends one record to `out`.
///
/// The IPC stream is written into its own buffer, then the header and payload
/// are appended. A payload over [`MAX_PAYLOAD`], or a batch whose decoded size is
/// over [`MAX_DECODED_BYTES`], is rejected and `out` is unchanged.
pub(crate) fn encode(seq: u64, batch: &RecordBatch, out: &mut Vec<u8>) -> Result<(), WalError> {
    encode_within(seq, batch, out, MAX_DECODED_BYTES)
}

/// [`encode`] with the decoded-size cap as a parameter, so a test can use a small one.
fn encode_within(
    seq: u64,
    batch: &RecordBatch,
    out: &mut Vec<u8>,
    max_decoded: u64,
) -> Result<(), WalError> {
    let mut payload = Vec::new();
    write_stream(batch.schema().as_ref(), &[batch], &mut payload)?;
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    if len > MAX_PAYLOAD {
        return Err(WalError::InvalidPayload {
            reason: "payload exceeds 256MiB",
        });
    }
    if !ipc_lengths_fit(&payload, max_decoded, Check::Write) {
        return Err(WalError::InvalidPayload {
            reason: "decoded size exceeds 1 GiB",
        });
    }
    let crc = checksum(seq, KIND_BATCH, &payload);
    out.reserve(HEADER_LEN + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.push(KIND_BATCH);
    out.extend_from_slice(&payload);
    Ok(())
}

/// Frames the next record. It checks the length, kind, and checksum, and does not parse IPC.
pub(crate) fn decode_next(buf: &[u8], offset: usize) -> Decoded {
    if offset == buf.len() {
        return Decoded::End;
    }
    if offset > buf.len() {
        return Decoded::Short { offset };
    }
    let rest = buf.len() - offset;
    if rest < 4 {
        return Decoded::Short { offset };
    }
    let payload_len = read_u32(buf, offset);
    if payload_len > MAX_PAYLOAD {
        return Decoded::Invalid {
            offset,
            reason: "payload exceeds 256MiB",
        };
    }
    if rest < HEADER_LEN {
        return Decoded::Short { offset };
    }
    let payload_len = payload_len as usize;
    let Some(record_len) = HEADER_LEN.checked_add(payload_len) else {
        return Decoded::Invalid {
            offset,
            reason: "payload length overflows",
        };
    };
    if rest < record_len {
        return Decoded::Short { offset };
    }
    let seq = read_u64(buf, offset + 8);
    let kind = buf[offset + 16];
    if kind != KIND_BATCH {
        return Decoded::Invalid {
            offset,
            reason: "invalid record kind",
        };
    }
    let payload_start = offset + HEADER_LEN;
    let payload_end = payload_start + payload_len;
    let payload = &buf[payload_start..payload_end];
    let stored = read_u32(buf, offset + 4);
    if stored != checksum(seq, kind, payload) {
        return Decoded::Invalid {
            offset,
            reason: "checksum mismatch",
        };
    }
    Decoded::Record {
        seq,
        payload: payload_start..payload_end,
        next: payload_end,
    }
}

/// Reads the single record batch inside a payload.
///
/// The stream must contain exactly one batch, and its fields must equal
/// `table`'s schema fields. Schema-level metadata is not compared.
///
/// Declared IPC message sizes are checked against the buffer first. Arrow's
/// reader allocates a body from the size in the message, so a corrupt length
/// must be rejected before that call. Buffer extents and LZ4 decompressed sizes
/// are checked the same way, because Arrow allocates each decompressed buffer
/// from its 8-byte prefix. A payload that fails any check is an
/// `invalid ipc stream`.
pub(crate) fn read_batch(payload: &[u8], table: Table) -> Result<RecordBatch, WalError> {
    if !ipc_lengths_fit(payload, MAX_DECODED_BYTES, Check::Read) {
        return Err(WalError::InvalidPayload {
            reason: "invalid ipc stream",
        });
    }
    // A panic in Arrow's decoder is a corrupt record (spec 03 §1, track R D-R-17).
    let cursor = std::io::Cursor::new(payload);
    let opened = catch_unwind(AssertUnwindSafe(|| StreamReader::try_new(cursor, None)))
        .map_err(|_| ipc_decoder_panicked())?;
    let mut reader = opened.map_err(|_| WalError::InvalidPayload {
        reason: "invalid ipc stream",
    })?;
    let Some(first) = next_batch(&mut reader)? else {
        return Err(WalError::InvalidPayload {
            reason: "ipc stream has no batch",
        });
    };
    let batch = first.map_err(|_| WalError::InvalidPayload {
        reason: "invalid ipc stream",
    })?;
    if next_batch(&mut reader)?.is_some() {
        return Err(WalError::InvalidPayload {
            reason: "ipc stream has more than one batch",
        });
    }
    if batch.schema().fields() != table.schema().fields() {
        return Err(WalError::InvalidPayload {
            reason: "schema does not match the table",
        });
    }
    Ok(batch)
}

/// `reader.next()`, with a decoder panic as a corrupt payload.
fn next_batch<R: Read>(
    reader: &mut StreamReader<R>,
) -> Result<Option<Result<RecordBatch, ArrowError>>, WalError> {
    catch_unwind(AssertUnwindSafe(|| reader.next())).map_err(|_| ipc_decoder_panicked())
}

fn ipc_decoder_panicked() -> WalError {
    WalError::InvalidPayload {
        reason: "ipc decoder panicked",
    }
}

/// What [`ipc_lengths_fit`] checks about each LZ4 frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    /// The writer, through [`encode_within`]. It has just compressed these frames
    /// itself, so the decoded size from each prefix is enough. Decoding every frame
    /// again would add a decompression to each WAL append, the hot ingest path.
    Write,
    /// Replay, through [`read_batch`]. A record read from disk is not trusted, so each
    /// frame must decode to exactly its prefix's size.
    Read,
}

/// Whether every declared length in the payload fits its bytes, and its LZ4 buffers
/// decode within `max_decoded` in total. Arrow allocates from these lengths, so
/// this runs before it. `check` decides whether the frames are decoded (see [`Check`]).
fn ipc_lengths_fit(payload: &[u8], max_decoded: u64, check: Check) -> bool {
    let mut decoded = 0;
    let mut offset = 0;
    while offset < payload.len() {
        if payload.len() - offset < 4 {
            return true;
        }
        let mut meta_len_bytes = [0; 4];
        meta_len_bytes.copy_from_slice(&payload[offset..offset + 4]);
        offset += 4;
        if meta_len_bytes == [0xff; 4] {
            if payload.len() - offset < 4 {
                return true;
            }
            meta_len_bytes.copy_from_slice(&payload[offset..offset + 4]);
            offset += 4;
        }
        let meta_len = i32::from_le_bytes(meta_len_bytes);
        if meta_len == 0 {
            return true;
        }
        let Ok(meta_len) = usize::try_from(meta_len) else {
            return false;
        };
        if meta_len > payload.len() - offset {
            return false;
        }
        let meta = &payload[offset..offset + meta_len];
        offset += meta_len;
        let Ok(message) = arrow::ipc::root_as_message(meta) else {
            return true;
        };
        let Ok(body_len) = usize::try_from(message.bodyLength()) else {
            return false;
        };
        if body_len > payload.len() - offset {
            return false;
        }
        if !buffers_fit(
            &message,
            &payload[offset..offset + body_len],
            &mut decoded,
            max_decoded,
            check,
        ) {
            return false;
        }
        offset += body_len;
    }
    true
}

/// Checks the buffers of a record batch message against its body. The message
/// must be a record batch or a dictionary batch, compressed with LZ4 frames
/// per buffer, or uncompressed. `decoded` sums the decompressed sizes over the
/// whole payload, and it must stay within `max_decoded`.
fn buffers_fit(
    message: &Message<'_>,
    body: &[u8],
    decoded: &mut u64,
    max_decoded: u64,
    check: Check,
) -> bool {
    let batch = if let Some(batch) = message.header_as_record_batch() {
        batch
    } else if let Some(dictionary) = message.header_as_dictionary_batch() {
        let Some(batch) = dictionary.data() else {
            return true;
        };
        batch
    } else {
        return true;
    };
    let compressed = match batch.compression() {
        None => false,
        Some(compression) => {
            if compression.codec() != CompressionType::LZ4_FRAME
                || compression.method() != BodyCompressionMethod::BUFFER
            {
                return false;
            }
            true
        }
    };
    let Some(buffers) = batch.buffers() else {
        return true;
    };
    for buffer in buffers.iter() {
        let (Ok(offset), Ok(length)) = (
            usize::try_from(buffer.offset()),
            usize::try_from(buffer.length()),
        ) else {
            return false;
        };
        let Some(end) = offset.checked_add(length) else {
            return false;
        };
        let Some(data) = body.get(offset..end) else {
            return false;
        };
        if compressed && length > 0 && !lz4_buffer_fits(data, decoded, max_decoded, check) {
            return false;
        }
    }
    true
}

/// Checks one LZ4 buffer. Its 8-byte prefix is the decompressed size: 0 means
/// empty, which Arrow allocates nothing for. -1 means stored raw, so its bytes
/// count toward the total as they are. Any other size must fit the compressed
/// bytes. On read, its LZ4 frame must also decode to exactly that size.
fn lz4_buffer_fits(data: &[u8], decoded: &mut u64, max_decoded: u64, check: Check) -> bool {
    let Some(prefix) = data.first_chunk::<8>() else {
        return false;
    };
    let size = i64::from_le_bytes(*prefix);
    if size == 0 {
        return true;
    }
    // `first_chunk` succeeded, so `data` holds at least the 8 prefix bytes.
    let frame = &data[8..];
    let Ok(compressed) = u64::try_from(frame.len()) else {
        return false;
    };
    let raw = size == -1;
    let declared = if raw {
        compressed
    } else {
        let Ok(size) = u64::try_from(size) else {
            return false;
        };
        if size
            > compressed
                .saturating_mul(LZ4_MAX_RATIO)
                .saturating_add(LZ4_SLACK)
        {
            return false;
        }
        size
    };
    match decoded.checked_add(declared) {
        Some(total) if total <= max_decoded => *decoded = total,
        _ => return false,
    }
    raw || check == Check::Write || lz4_frame_decodes_to(frame, declared)
}

/// Whether `frame` is an LZ4 frame that decodes to exactly `size` bytes. It decodes
/// into a counting sink, so memory is bounded by the frame's block size rather than
/// by `size`. It reads one byte past `size`, so a longer frame is caught as well.
fn lz4_frame_decodes_to(frame: &[u8], size: u64) -> bool {
    let mut decoder = lz4_flex::frame::FrameDecoder::new(frame).take(size.saturating_add(1));
    matches!(std::io::copy(&mut decoder, &mut std::io::sink()), Ok(n) if n == size)
}

fn write_stream(
    schema: &Schema,
    batches: &[&RecordBatch],
    out: &mut Vec<u8>,
) -> Result<(), WalError> {
    let options = IpcWriteOptions::default()
        .try_with_compression(Some(CompressionType::LZ4_FRAME))
        .map_err(WalError::Encode)?;
    let mut writer =
        StreamWriter::try_new_with_options(out, schema, options).map_err(WalError::Encode)?;
    for batch in batches {
        writer.write(batch).map_err(WalError::Encode)?;
    }
    writer.finish().map_err(WalError::Encode)?;
    Ok(())
}

/// Appends one framed record whose payload is not necessarily IPC.
///
/// Replay tests use this to build a checksum-valid record that `read_batch` rejects.
/// The fuzz entries use it to frame segments with a real checksum.
#[cfg(any(test, fuzzing))]
pub(crate) fn push_framed(buf: &mut Vec<u8>, seq: u64, payload: &[u8]) {
    // A capped length fails the length check on decode; it never panics.
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(&checksum(seq, KIND_BATCH, payload).to_le_bytes());
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.push(KIND_BATCH);
    buf.extend_from_slice(payload);
}

fn checksum(seq: u64, kind: u8, payload: &[u8]) -> u32 {
    let mut crc = crc32c::crc32c(&seq.to_le_bytes());
    crc = crc32c::crc32c_append(crc, &[kind]);
    crc32c::crc32c_append(crc, payload)
}

fn read_u32(buf: &[u8], at: usize) -> u32 {
    let mut bytes = [0; 4];
    bytes.copy_from_slice(&buf[at..at + 4]);
    u32::from_le_bytes(bytes)
}

fn read_u64(buf: &[u8], at: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&buf[at..at + 8]);
    u64::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use arrow::array::{Int32Array, RecordBatch};
    use arrow::datatypes::{DataType, Field, Schema};
    use cetra_core::batch::{scores_to_record_batch, spans_to_record_batch};
    use cetra_core::enums::{ScoreSource, SpanKind, SpanStatus};
    use cetra_core::ids::{ProjectId, SpanId, TraceId};
    use cetra_core::score::{Score, ScoreTarget, ScoreValue};
    use cetra_core::span::{Span, SpanEvent};
    use proptest::prelude::*;

    use crate::wal::{sample_span_batch, segment_name};

    use super::*;

    fn project() -> ProjectId {
        "018f6b4e-7c1a-7c1a-8c1a-000000000001".parse().unwrap()
    }

    fn trace() -> TraceId {
        "4bf92f3577b34da6a3ce929d0e0e4736".parse().unwrap()
    }

    fn span_id(last: u8) -> SpanId {
        let mut bytes = [0; 8];
        bytes[7] = last;
        SpanId::from_bytes(bytes).unwrap()
    }

    fn span(name: &str, with_nested: bool) -> Span {
        let mut metadata = BTreeMap::new();
        let mut attributes = BTreeMap::new();
        if with_nested {
            metadata.insert("plan".to_owned(), "\"pro\"".to_owned());
            attributes.insert("http.method".to_owned(), "\"GET\"".to_owned());
        }
        Span {
            project_id: project(),
            trace_id: trace(),
            span_id: span_id(if with_nested { 2 } else { 1 }),
            parent_span_id: None,
            name: name.to_owned(),
            span_kind: SpanKind::Llm,
            start_time: 1_700_000_000_000_000_000,
            end_time: 1_700_000_000_100_000_000,
            status: SpanStatus::Ok,
            status_message: None,
            service_name: None,
            session_id: None,
            user_id: None,
            environment: None,
            release: None,
            tags: if with_nested {
                vec!["beta".to_owned(), "eu".to_owned()]
            } else {
                Vec::new()
            },
            metadata,
            provider: None,
            request_model: None,
            response_model: None,
            operation: None,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            total_tokens: None,
            cost_input: None,
            cost_output: None,
            cost_total: None,
            cost_source: None,
            ttft_ns: None,
            request_params: None,
            input: None,
            input_format: None,
            output: None,
            output_format: None,
            input_preview: None,
            output_preview: None,
            system_instructions: None,
            tool_definitions: None,
            attributes,
            resource_attributes: BTreeMap::new(),
            scope_name: None,
            scope_version: None,
            events: if with_nested {
                vec![SpanEvent {
                    time: 1_700_000_000_000_000_000,
                    name: "exception".to_owned(),
                    attributes: None,
                }]
            } else {
                Vec::new()
            },
            links: Vec::new(),
            media_refs: Vec::new(),
            truncated: Vec::new(),
            conventions: Vec::new(),
            ingest_time: 1_700_000_000_200_000_000,
            schema_version: 1,
        }
    }

    fn score(with_map: bool) -> Score {
        let mut metadata = BTreeMap::new();
        if with_map {
            metadata.insert("judge".to_owned(), "small".to_owned());
        }
        Score {
            project_id: project(),
            score_id: "018f6b4e-7c1a-7c1a-8c1a-0000000000ab".parse().unwrap(),
            target: ScoreTarget::Trace { trace_id: trace() },
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
            author_user_id: None,
            metadata,
            created_at: 1_700_000_000_000_000_000,
            updated_at: 1_700_000_001_000_000_000,
            deleted: false,
            ingest_time: 1_700_000_002_000_000_000,
        }
    }

    fn spans_batch() -> RecordBatch {
        spans_to_record_batch(&[span("root", false), span("chat", true)]).unwrap()
    }

    fn scores_batch() -> RecordBatch {
        scores_to_record_batch(&[score(false), score(true)]).unwrap()
    }

    fn round_trip(table: Table, batch: &RecordBatch) {
        let mut buf = Vec::new();
        encode(7, batch, &mut buf).unwrap();
        match decode_next(&buf, 0) {
            Decoded::Record { seq, payload, next } => {
                assert_eq!(seq, 7);
                assert_eq!(next, buf.len());
                assert_eq!(read_batch(&buf[payload], table).unwrap(), *batch);
            }
            other => panic!("expected a record, got {other:?}"),
        }
        assert_eq!(decode_next(&buf, buf.len()), Decoded::End);
    }

    #[test]
    fn table_names_match_the_directories() {
        assert_eq!(Table::Spans.name(), "spans");
        assert_eq!(Table::Scores.name(), "scores");
    }

    #[test]
    fn spans_and_scores_round_trip() {
        round_trip(Table::Spans, &spans_batch());
        round_trip(Table::Scores, &scores_batch());
    }

    #[test]
    fn several_records_decode_in_order() {
        let batch = spans_batch();
        let mut buf = Vec::new();
        encode(1, &batch, &mut buf).unwrap();
        encode(2, &batch, &mut buf).unwrap();
        encode(3, &batch, &mut buf).unwrap();
        let mut offset = 0;
        for expected in 1..=3 {
            match decode_next(&buf, offset) {
                Decoded::Record { seq, next, .. } => {
                    assert_eq!(seq, expected);
                    offset = next;
                }
                other => panic!("record {expected}: {other:?}"),
            }
        }
        assert_eq!(decode_next(&buf, offset), Decoded::End);
    }

    fn push_record(buf: &mut Vec<u8>, seq: u64, payload: &[u8]) {
        push_framed(buf, seq, payload);
    }

    #[test]
    fn every_truncation_point_keeps_only_complete_records() {
        let payload = [0x11_u8; 8];
        let mut buf = Vec::new();
        push_record(&mut buf, 1, &payload);
        let first_end = buf.len();
        push_record(&mut buf, 2, &payload);

        for len in 0..=buf.len() {
            let slice = &buf[..len];
            let mut offset = 0;
            let mut seqs = Vec::new();
            loop {
                match decode_next(slice, offset) {
                    Decoded::Record { seq, next, .. } => {
                        assert!(next > offset);
                        offset = next;
                        seqs.push(seq);
                    }
                    Decoded::End => {
                        assert_eq!(offset, len);
                        assert!(offset == 0 || offset == first_end || offset == buf.len());
                        break;
                    }
                    Decoded::Short { offset: short } => {
                        assert_eq!(short, offset);
                        break;
                    }
                    Decoded::Invalid { offset, reason } => {
                        panic!("truncation at {len} was invalid at {offset}: {reason}")
                    }
                }
            }
            let complete = usize::from(len >= first_end) + usize::from(len == buf.len());
            assert_eq!(seqs, (1..=complete as u64).collect::<Vec<_>>());
        }
    }

    #[test]
    fn flipping_one_bit_never_yields_a_record() {
        // A short payload keeps the exhaustive check cheap. The framing and
        // checksum do not depend on the IPC bytes being a real batch.
        let payload = [0x5a_u8; 24];
        let seq = 4_u64;
        let mut buf = Vec::new();
        push_record(&mut buf, seq, &payload);
        assert!(matches!(
            decode_next(&buf, 0),
            Decoded::Record { seq: 4, .. }
        ));

        let mut corrupted = buf.clone();
        for bit in 0..corrupted.len() * 8 {
            let byte = bit / 8;
            let mask = 1 << (bit % 8);
            corrupted[byte] ^= mask;
            match decode_next(&corrupted, 0) {
                Decoded::Record { .. } => panic!("bit {bit} decoded as a record"),
                Decoded::Invalid { .. } => {}
                Decoded::Short { .. } | Decoded::End => {
                    assert!(bit < 32, "bit {bit} is outside payload_len");
                }
            }
            corrupted[byte] ^= mask;
        }
    }

    #[test]
    fn a_bit_flip_in_a_real_spans_record_is_invalid() {
        let mut buf = Vec::new();
        encode(4, &spans_batch(), &mut buf).unwrap();
        for index in [0, 5, 16, buf.len() / 2, buf.len() - 1] {
            buf[index] ^= 0x01;
            assert!(
                matches!(
                    decode_next(&buf, 0),
                    Decoded::Invalid { .. } | Decoded::Short { .. }
                ),
                "index {index}"
            );
            buf[index] ^= 0x01;
        }
    }

    #[test]
    fn kind_other_than_batch_is_invalid() {
        let mut buf = Vec::new();
        encode(1, &spans_batch(), &mut buf).unwrap();
        buf[16] = 2;
        let seq = 1_u64;
        let payload = &buf[HEADER_LEN..];
        let crc = checksum(seq, 2, payload);
        buf[4..8].copy_from_slice(&crc.to_le_bytes());
        match decode_next(&buf, 0) {
            Decoded::Invalid { reason, .. } => assert_eq!(reason, "invalid record kind"),
            other => panic!("expected invalid kind, got {other:?}"),
        }
    }

    #[test]
    fn oversized_length_is_invalid_without_a_body() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        buf.extend_from_slice(&[0; 13]);
        match decode_next(&buf, 0) {
            Decoded::Invalid { reason, .. } => assert_eq!(reason, "payload exceeds 256MiB"),
            other => panic!("expected oversized invalid, got {other:?}"),
        }
    }

    #[test]
    fn read_batch_rejects_the_wrong_number_of_batches_and_the_wrong_schema() {
        let spans = spans_batch();
        let mut two = Vec::new();
        write_stream(spans.schema().as_ref(), &[&spans, &spans], &mut two).unwrap();
        let error = read_batch(&two, Table::Spans).unwrap_err();
        assert!(error.to_string().contains("more than one batch"));

        let mut zero = Vec::new();
        write_stream(spans.schema().as_ref(), &[], &mut zero).unwrap();
        let error = read_batch(&zero, Table::Spans).unwrap_err();
        assert!(error.to_string().contains("no batch"));

        let schema = Arc::new(Schema::new(vec![Field::new("x", DataType::Int32, false)]));
        let other = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![Arc::new(Int32Array::from(vec![1]))],
        )
        .unwrap();
        let mut wrong = Vec::new();
        write_stream(schema.as_ref(), &[&other], &mut wrong).unwrap();
        let error = read_batch(&wrong, Table::Spans).unwrap_err();
        assert!(error.to_string().contains("schema does not match"));
    }

    /// Where one record batch sits in an IPC stream payload.
    struct BatchLayout {
        /// The message's flatbuffer metadata.
        meta: Range<usize>,
        /// The message's body.
        body: Range<usize>,
        /// Each buffer's (offset, length), relative to the body.
        buffers: Vec<(i64, i64)>,
    }

    /// Walks the stream's messages to its first record batch.
    fn batch_layout(payload: &[u8]) -> BatchLayout {
        let mut offset = 0;
        loop {
            let mut len = [0; 4];
            len.copy_from_slice(&payload[offset..offset + 4]);
            offset += 4;
            if len == [0xff; 4] {
                len.copy_from_slice(&payload[offset..offset + 4]);
                offset += 4;
            }
            let meta_len = usize::try_from(i32::from_le_bytes(len)).unwrap();
            let meta = offset..offset + meta_len;
            offset += meta_len;
            let message = arrow::ipc::root_as_message(&payload[meta.clone()]).unwrap();
            let body_len = usize::try_from(message.bodyLength()).unwrap();
            let body = offset..offset + body_len;
            if let Some(batch) = message.header_as_record_batch() {
                let buffers = batch
                    .buffers()
                    .unwrap()
                    .iter()
                    .map(|buffer| (buffer.offset(), buffer.length()))
                    .collect();
                return BatchLayout {
                    meta,
                    body,
                    buffers,
                };
            }
            offset += body_len;
        }
    }

    /// The first buffer that Arrow would decompress: an LZ4 prefix above zero.
    /// Returns its index, and its absolute position in the payload.
    fn first_lz4_buffer(payload: &[u8]) -> (usize, usize) {
        let layout = batch_layout(payload);
        layout
            .buffers
            .iter()
            .enumerate()
            .find_map(|(index, (offset, length))| {
                let at = layout.body.start + usize::try_from(*offset).unwrap();
                let prefix = i64::from_le_bytes(payload[at..at + 8].try_into().unwrap());
                (*length > 8 && prefix > 0).then_some((index, at))
            })
            .expect("the record has a compressed buffer")
    }

    /// Overwrites the LZ4 prefix of the first compressed buffer.
    fn set_first_lz4_prefix(payload: &mut [u8], value: i64) {
        let (_, at) = first_lz4_buffer(payload);
        payload[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// Overwrites one buffer's length in the flatbuffer metadata. Non-empty
    /// buffers have distinct offsets, so the (offset, length) pair is unique.
    fn set_buffer_length(payload: &mut [u8], index: usize, length: i64) {
        let layout = batch_layout(payload);
        let (offset, old) = layout.buffers[index];
        let mut pair = Vec::new();
        pair.extend_from_slice(&offset.to_le_bytes());
        pair.extend_from_slice(&old.to_le_bytes());
        let meta = &payload[layout.meta.clone()];
        let found: Vec<usize> = meta
            .windows(16)
            .enumerate()
            .filter(|(_, window)| *window == pair.as_slice())
            .map(|(at, _)| at)
            .collect();
        assert_eq!(
            found.len(),
            1,
            "buffer {index} appears once in its metadata"
        );
        let at = layout.meta.start + found[0] + 8;
        payload[at..at + 8].copy_from_slice(&length.to_le_bytes());
    }

    /// The encoded payload of one record, as the segment stores it.
    fn payload_of(table_batch: &RecordBatch, seq: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        encode(seq, table_batch, &mut buf).unwrap();
        let Decoded::Record { payload, .. } = decode_next(&buf, 0) else {
            panic!("a re-encoded record frames");
        };
        buf[payload].to_vec()
    }

    fn is_invalid_payload(result: &Result<RecordBatch, WalError>) -> bool {
        matches!(result, Err(WalError::InvalidPayload { .. }))
    }

    #[test]
    fn a_lying_lz4_length_is_a_corrupt_record() {
        for (table, batch) in [
            (Table::Spans, spans_batch()),
            (Table::Scores, scores_batch()),
        ] {
            let good = payload_of(&batch, 1);
            assert!(
                read_batch(&good, table).is_ok(),
                "the unmodified record decodes"
            );

            let mut bad = good.clone();
            // 1 TiB: the allocation Arrow would make from this prefix.
            set_first_lz4_prefix(&mut bad, 1 << 40);
            assert!(!ipc_lengths_fit(&bad, MAX_DECODED_BYTES, Check::Read));
            assert!(is_invalid_payload(&read_batch(&bad, table)));
        }
    }

    #[test]
    fn an_lz4_size_past_the_ratio_bound_is_corrupt() {
        let good = payload_of(&spans_batch(), 1);
        let (index, at) = first_lz4_buffer(&good);
        let length = usize::try_from(batch_layout(&good).buffers[index].1).unwrap();
        let bound = u64::try_from(length - 8).unwrap() * LZ4_MAX_RATIO + LZ4_SLACK;

        // At the bound the ratio check passes, and the frame check then decides. The
        // prefix here is not the frame's real size, so that is corrupt; one byte past
        // the bound is corrupt by the ratio alone.
        let mut past = good;
        past[at..at + 8].copy_from_slice(&i64::try_from(bound + 1).unwrap().to_le_bytes());
        assert!(!ipc_lengths_fit(&past, MAX_DECODED_BYTES, Check::Read));
    }

    #[test]
    fn a_stored_raw_or_empty_lz4_buffer_still_fits() {
        // One short span: every buffer is smaller than an LZ4 frame, so Arrow
        // stores it raw with the -1 prefix.
        let payload = payload_of(&sample_span_batch(), 1);
        let layout = batch_layout(&payload);
        let raw = layout.buffers.iter().any(|(offset, length)| {
            let at = layout.body.start + usize::try_from(*offset).unwrap();
            *length > 0 && i64::from_le_bytes(payload[at..at + 8].try_into().unwrap()) == -1
        });
        assert!(raw, "a one-span record stores a buffer raw");
        assert!(read_batch(&payload, Table::Spans).is_ok());

        // A prefix of 0 is an empty buffer, which needs no allocation.
        let mut empty = payload_of(&spans_batch(), 1);
        let (_, at) = first_lz4_buffer(&empty);
        empty[at..at + 8].copy_from_slice(&0_i64.to_le_bytes());
        assert!(ipc_lengths_fit(&empty, MAX_DECODED_BYTES, Check::Read));
    }

    /// The decoded bytes the cap counts for a payload: each LZ4 buffer's declared
    /// size, or the bytes of a stored-raw buffer.
    fn decoded_total(payload: &[u8]) -> u64 {
        let layout = batch_layout(payload);
        layout
            .buffers
            .iter()
            .map(|(offset, length)| {
                if *length == 0 {
                    return 0;
                }
                let at = layout.body.start + usize::try_from(*offset).unwrap();
                match i64::from_le_bytes(payload[at..at + 8].try_into().unwrap()) {
                    0 => 0,
                    -1 => u64::try_from(*length - 8).unwrap(),
                    size => u64::try_from(size).unwrap(),
                }
            })
            .sum()
    }

    #[test]
    fn the_decoded_cap_counts_raw_and_lz4_buffers_exactly() {
        for batch in [spans_batch(), sample_span_batch()] {
            let payload = payload_of(&batch, 1);
            let total = decoded_total(&payload);
            assert!(total > 0);
            assert!(ipc_lengths_fit(&payload, total, Check::Read));
            assert!(!ipc_lengths_fit(&payload, total - 1, Check::Read));
        }
    }

    #[test]
    fn a_batch_over_the_decoded_cap_is_not_written() {
        let batch = sample_span_batch();
        let total = decoded_total(&payload_of(&batch, 1));

        let mut out = vec![0xaa];
        assert!(matches!(
            encode_within(1, &batch, &mut out, total - 1),
            Err(WalError::InvalidPayload {
                reason: "decoded size exceeds 1 GiB"
            })
        ));
        assert_eq!(out, [0xaa], "a rejected batch leaves the buffer as it was");

        encode_within(1, &batch, &mut out, total).unwrap();
        let Decoded::Record { payload, .. } = decode_next(&out, 1) else {
            panic!("the accepted batch frames after the existing byte");
        };
        assert!(read_batch(&out[payload], Table::Spans).is_ok());
    }

    #[test]
    fn an_lz4_prefix_that_disagrees_with_its_frame_is_corrupt() {
        let good = payload_of(&spans_batch(), 1);
        let (_, at) = first_lz4_buffer(&good);
        let claimed = i64::from_le_bytes(good[at..at + 8].try_into().unwrap());
        assert!(claimed > 1);
        assert!(ipc_lengths_fit(&good, MAX_DECODED_BYTES, Check::Read));

        // A prefix of 1 is far inside the ratio bound, so only the frame check can
        // see that the frame decodes to more.
        let mut short = good.clone();
        short[at..at + 8].copy_from_slice(&1_i64.to_le_bytes());
        assert!(!ipc_lengths_fit(&short, MAX_DECODED_BYTES, Check::Read));
        assert!(is_invalid_payload(&read_batch(&short, Table::Spans)));

        // A frame that decodes to less than its prefix claims is corrupt too.
        let mut long = good;
        long[at..at + 8].copy_from_slice(&(claimed + 1).to_le_bytes());
        assert!(!ipc_lengths_fit(&long, MAX_DECODED_BYTES, Check::Read));
        assert!(is_invalid_payload(&read_batch(&long, Table::Spans)));
    }

    #[test]
    fn the_write_check_skips_the_frame_decode_and_the_read_check_does_not() {
        let good = payload_of(&spans_batch(), 1);
        let (index, at) = first_lz4_buffer(&good);
        assert!(ipc_lengths_fit(&good, MAX_DECODED_BYTES, Check::Write));
        assert!(ipc_lengths_fit(&good, MAX_DECODED_BYTES, Check::Read));

        // A prefix of 1 disagrees with the frame the writer left. The write check
        // only sums prefixes, so it accepts; the read check decodes and rejects.
        let mut lying = good.clone();
        lying[at..at + 8].copy_from_slice(&1_i64.to_le_bytes());
        assert!(ipc_lengths_fit(&lying, MAX_DECODED_BYTES, Check::Write));
        assert!(!ipc_lengths_fit(&lying, MAX_DECODED_BYTES, Check::Read));
        assert!(is_invalid_payload(&read_batch(&lying, Table::Spans)));

        // The write check still rejects what needs no decode: a prefix past the
        // ratio bound.
        let length = usize::try_from(batch_layout(&good).buffers[index].1).unwrap();
        let bound = u64::try_from(length - 8).unwrap() * LZ4_MAX_RATIO + LZ4_SLACK;
        let mut past = good;
        past[at..at + 8].copy_from_slice(&i64::try_from(bound + 1).unwrap().to_le_bytes());
        assert!(!ipc_lengths_fit(&past, MAX_DECODED_BYTES, Check::Write));
    }

    #[test]
    fn a_buffer_extent_past_the_body_is_corrupt() {
        let good = payload_of(&spans_batch(), 1);
        let layout = batch_layout(&good);
        let index = layout
            .buffers
            .iter()
            .position(|(_, length)| *length > 0)
            .expect("the record has a non-empty buffer");

        let mut bad = good;
        set_buffer_length(&mut bad, index, 1 << 40);
        assert!(!ipc_lengths_fit(&bad, MAX_DECODED_BYTES, Check::Read));
        assert!(is_invalid_payload(&read_batch(&bad, Table::Spans)));
    }

    #[test]
    fn a_zstd_record_is_corrupt() {
        let batch = spans_batch();
        let options = IpcWriteOptions::default()
            .try_with_compression(Some(CompressionType::ZSTD))
            .unwrap();
        let mut zstd = Vec::new();
        let mut writer =
            StreamWriter::try_new_with_options(&mut zstd, batch.schema().as_ref(), options)
                .unwrap();
        writer.write(&batch).unwrap();
        writer.finish().unwrap();
        assert!(!ipc_lengths_fit(&zstd, MAX_DECODED_BYTES, Check::Read));
        assert!(is_invalid_payload(&read_batch(&zstd, Table::Spans)));
    }

    #[test]
    fn a_crc_valid_record_with_a_lying_length_is_corrupt_per_skip_corrupt() {
        let good = payload_of(&spans_batch(), 1);
        let mut bad = payload_of(&spans_batch(), 2);
        set_first_lz4_prefix(&mut bad, 1 << 40);

        // Every frame has a real checksum, so only the IPC check can reject seq 2.
        let mut segment = Vec::new();
        push_framed(&mut segment, 1, &good);
        push_framed(&mut segment, 2, &bad);
        push_framed(&mut segment, 3, &good);
        let dir = tempfile::tempdir().unwrap();
        let segment = crate::wal::segment::with_header(&segment);
        std::fs::write(dir.path().join(segment_name(1)), &segment).unwrap();

        let hooks = crate::hooks::Hooks::default();
        let strict = crate::recover_table(dir.path(), Table::Spans, 0, false, u64::MAX, &hooks);
        assert!(
            matches!(strict, Err(crate::error::OpenError::WalCorrupt { .. })),
            "a corrupt record stops replay as a corrupt WAL"
        );

        // Seqs 1 and 3 replay; seq 2 is the one skipped.
        let skipped =
            crate::recover_table(dir.path(), Table::Spans, 0, true, u64::MAX, &hooks).unwrap();
        assert_eq!(skipped.records, 2, "one of three frames is skipped");
    }

    proptest! {
        #[test]
        fn random_bytes_do_not_panic(buf in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let _ = read_batch(&buf, Table::Spans);
            let _ = read_batch(&buf, Table::Scores);
            let mut offset = 0;
            for _ in 0..=buf.len() {
                match decode_next(&buf, offset) {
                    Decoded::Record { payload, next, .. } => {
                        assert!(next > offset);
                        let _ = read_batch(&buf[payload], Table::Spans);
                        offset = next;
                    }
                    Decoded::End | Decoded::Short { .. } | Decoded::Invalid { .. } => break,
                }
            }
        }
    }
}
