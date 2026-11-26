//! WAL record framing.
//!
//! Spec 03 §1. Integers are little-endian. The checksum covers `seq` as 8
//! little-endian bytes, then `kind` as one byte, then the payload.

use std::ops::Range;

use cetra_column::{Batch, Column, FlatData};

use crate::error::WalError;
use crate::wal::Table;

/// `payload_len` + checksum + `seq` + `kind`.
pub(crate) const HEADER_LEN: usize = 17;

/// Reject larger records on write, and treat them as invalid on read.
///
/// A corrupt length must not turn into a multi-gigabyte allocation.
pub(crate) const MAX_PAYLOAD: u32 = 256 * 1024 * 1024;

const KIND_BATCH: u8 = 1;

/// One step of [`decode_next`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decoded {
    /// A framed record. `payload` indexes the batch frame bytes inside the buffer.
    Record {
        /// WAL sequence number.
        seq: u64,
        /// Byte range of the batch frame payload.
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
/// The batch frame is written into its own buffer, then the outer 17-byte header
/// and payload are appended. A payload over [`MAX_PAYLOAD`], or a batch whose
/// decoded size exceeds `MAX_DECODED_BYTES`, is rejected and `out` is unchanged.
pub(crate) fn encode(seq: u64, batch: &Batch, out: &mut Vec<u8>) -> Result<(), WalError> {
    encode_within(seq, batch, out, cetra_column::frame::MAX_DECODED_BYTES)
}

/// [`encode`] with the decoded-size cap as a parameter, so a test can use a small one.
fn encode_within(
    seq: u64,
    batch: &Batch,
    out: &mut Vec<u8>,
    max_decoded: u32,
) -> Result<(), WalError> {
    let mut payload = Vec::new();
    cetra_column::frame::encode_batch_with_limit(batch, &mut payload, max_decoded)
        .map_err(WalError::from_column)?;
    let Ok(len) = u32::try_from(payload.len()) else {
        return Err(WalError::InvalidPayload {
            reason: "payload exceeds 256MiB",
        });
    };
    if len > MAX_PAYLOAD {
        return Err(WalError::InvalidPayload {
            reason: "payload exceeds 256MiB",
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

/// Frames the next record. It checks the length, kind, and checksum, and does not parse the batch frame.
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

/// Reads the single batch inside a payload.
///
/// Rejects payloads larger than [`MAX_PAYLOAD`], verifies that the decoded
/// schema matches `table`'s columnar schema, and recursively verifies that
/// every column (including nested `List` child columns) uses `Column::Flat`.
pub(crate) fn read_batch(payload: &[u8], table: Table) -> Result<Batch, WalError> {
    if payload.len() > MAX_PAYLOAD as usize {
        return Err(WalError::InvalidPayload {
            reason: "payload exceeds 256MiB",
        });
    }
    let batch = cetra_column::frame::decode_batch(payload, Some(&table.column_schema()), None)
        .map_err(WalError::from_column)?;
    if !batch.columns().iter().all(is_all_flat) {
        return Err(WalError::InvalidPayload {
            reason: "non-flat column encoding in WAL record",
        });
    }
    Ok(batch)
}

fn is_all_flat(col: &Column) -> bool {
    match col {
        Column::Flat(fc) => match fc.data() {
            FlatData::List { values, .. } => is_all_flat(values),
            _ => true,
        },
        Column::Const { .. } | Column::Dict(_) | Column::Rle(_) => false,
    }
}

/// Appends one framed record whose payload is not necessarily a valid batch frame.
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

    use cetra_column::{
        DataType, DictColumn, Field, FlatColumn, FlatData, RleColumn, ScalarValue, Schema,
    };
    use cetra_core::batch::{scores_to_record_batch, spans_to_record_batch};
    use cetra_core::enums::{ScoreSource, SpanKind, SpanStatus};
    use cetra_core::ids::{ProjectId, SpanId, TraceId};
    use cetra_core::schema::spans as span_cols;
    use cetra_core::score::{Score, ScoreTarget, ScoreValue};
    use cetra_core::span::{Span, SpanEvent};
    use proptest::prelude::*;

    use crate::column_bridge::record_batch_to_column;
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
            prompt_name: None,
            prompt_version: None,
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
            queue_id: None,
            author_user_id: None,
            metadata,
            created_at: 1_700_000_000_000_000_000,
            updated_at: 1_700_000_001_000_000_000,
            deleted: false,
            ingest_time: 1_700_000_002_000_000_000,
        }
    }

    fn spans_batch() -> Batch {
        let rb = spans_to_record_batch(&[span("root", false), span("chat", true)]).unwrap();
        record_batch_to_column(&rb).unwrap()
    }

    fn scores_batch() -> Batch {
        let rb = scores_to_record_batch(&[score(false), score(true)]).unwrap();
        record_batch_to_column(&rb).unwrap()
    }

    fn sample_span_col_batch() -> Batch {
        record_batch_to_column(&sample_span_batch()).unwrap()
    }

    fn round_trip(table: Table, batch: &Batch) {
        let mut buf = Vec::new();
        encode(7, batch, &mut buf).unwrap();
        match decode_next(&buf, 0) {
            Decoded::Record { seq, payload, next } => {
                assert_eq!(seq, 7);
                assert_eq!(next, buf.len());
                let decoded = read_batch(&buf[payload], table).unwrap();
                assert!(decoded.semantic_eq(batch));
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

    /// The encoded payload of one record, as the segment stores it.
    fn payload_of(table_batch: &Batch, seq: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        encode(seq, table_batch, &mut buf).unwrap();
        let Decoded::Record { payload, .. } = decode_next(&buf, 0) else {
            panic!("a re-encoded record frames");
        };
        buf[payload].to_vec()
    }

    fn is_invalid_payload(result: &Result<Batch, WalError>) -> bool {
        matches!(result, Err(WalError::InvalidPayload { .. }))
    }

    #[test]
    fn read_batch_rejects_wrong_schema_and_corrupt_frames() {
        let empty: &[u8] = &[];
        assert!(is_invalid_payload(&read_batch(empty, Table::Spans)));

        let spans_payload = payload_of(&spans_batch(), 1);
        assert!(is_invalid_payload(&read_batch(
            &spans_payload,
            Table::Scores
        )));

        let schema = Schema::try_new_arc(vec![Field::new("x", DataType::I64, false)]).unwrap();
        let col = Column::Flat(FlatColumn::try_new(FlatData::I64(vec![1]), None).unwrap());
        let wrong = Batch::try_new_all(schema, vec![col], 1).unwrap();
        let mut wrong_payload = Vec::new();
        cetra_column::frame::encode_batch(&wrong, &mut wrong_payload).unwrap();
        assert!(is_invalid_payload(&read_batch(
            &wrong_payload,
            Table::Spans
        )));

        let mut flipped = spans_payload;
        let mid = flipped.len() / 2;
        flipped[mid] ^= 0xff;
        assert!(is_invalid_payload(&read_batch(&flipped, Table::Spans)));
    }

    #[test]
    fn a_batch_over_the_decoded_cap_is_not_written() {
        let batch = sample_span_col_batch();
        let mut out = vec![0xaa];
        assert!(matches!(
            encode_within(1, &batch, &mut out, 64),
            Err(WalError::InvalidPayload { .. })
        ));
        assert_eq!(out, [0xaa], "a rejected batch leaves the buffer as it was");

        encode(1, &batch, &mut out).unwrap();
        let Decoded::Record { payload, .. } = decode_next(&out, 1) else {
            panic!("the accepted batch frames after the existing byte");
        };
        assert!(read_batch(&out[payload], Table::Spans).is_ok());
    }

    #[test]
    fn read_batch_rejects_top_level_const_u32_max_and_nested_list_const_bombs() {
        let base = sample_span_col_batch();
        let schema = Arc::clone(base.schema());

        // 1. Top-level Column::Const(len = u32::MAX) across all columns -> ~500B bomb.
        let mut const_cols = Vec::with_capacity(schema.len());
        for col in base.columns() {
            let scalar = col.get_scalar(0).unwrap();
            const_cols.push(Column::try_new_const(scalar, u32::MAX as usize).unwrap());
        }
        let top_bomb =
            Batch::try_new_all(Arc::clone(&schema), const_cols, u32::MAX as usize).unwrap();
        let mut top_payload = Vec::new();
        cetra_column::frame::encode_batch(&top_bomb, &mut top_payload).unwrap();
        assert!(top_payload.len() < 2048);
        let err = read_batch(&top_payload, Table::Spans).unwrap_err();
        assert!(
            matches!(err, WalError::InvalidPayload { reason } if reason == "non-flat column encoding in WAL record"),
            "expected non-flat rejection for top-level Const bomb, got {err:?}"
        );

        // 2. Nested FlatData::List { offsets: [0, u32::MAX], values: Const(u32::MAX) } bomb.
        let tags_idx = schema.index_of(span_cols::TAGS).unwrap();
        let child = Box::new(
            Column::try_new_const(ScalarValue::Utf8("x".to_owned()), u32::MAX as usize).unwrap(),
        );
        let list_bomb_col = Column::Flat(
            FlatColumn::try_new(
                FlatData::List {
                    offsets: vec![0, u32::MAX],
                    values: child,
                    item_type: DataType::Utf8,
                },
                None,
            )
            .unwrap(),
        );
        let mut nested_cols = base.columns().to_vec();
        nested_cols[tags_idx] = list_bomb_col;
        let nested_bomb = Batch::try_new_all(Arc::clone(&schema), nested_cols, 1).unwrap();
        let mut nested_payload = Vec::new();
        cetra_column::frame::encode_batch(&nested_bomb, &mut nested_payload).unwrap();
        let err = read_batch(&nested_payload, Table::Spans).unwrap_err();
        assert!(
            matches!(err, WalError::InvalidPayload { reason } if reason == "non-flat column encoding in WAL record"),
            "expected non-flat rejection for nested List(Const) bomb, got {err:?}"
        );

        // 3. Dict and Rle encodings are also rejected in WAL records.
        let name_idx = schema.index_of(span_cols::NAME).unwrap();
        let mut dict_cols = base.columns().to_vec();
        let dict_vals = FlatColumn::try_new(
            FlatData::Utf8(cetra_column::Utf8Data::try_from_slices(["root"]).unwrap()),
            None,
        )
        .unwrap();
        dict_cols[name_idx] = Column::Dict(DictColumn::try_new(dict_vals, vec![0], None).unwrap());
        let dict_batch = Batch::try_new_all(Arc::clone(&schema), dict_cols, 1).unwrap();
        let mut dict_payload = Vec::new();
        cetra_column::frame::encode_batch(&dict_batch, &mut dict_payload).unwrap();
        assert!(is_invalid_payload(&read_batch(&dict_payload, Table::Spans)));

        let mut rle_cols = base.columns().to_vec();
        let rle_vals = FlatColumn::try_new(
            FlatData::Utf8(cetra_column::Utf8Data::try_from_slices(["root"]).unwrap()),
            None,
        )
        .unwrap();
        rle_cols[name_idx] = Column::Rle(RleColumn::try_new(rle_vals, vec![1]).unwrap());
        let rle_batch = Batch::try_new_all(schema, rle_cols, 1).unwrap();
        let mut rle_payload = Vec::new();
        cetra_column::frame::encode_batch(&rle_batch, &mut rle_payload).unwrap();
        assert!(is_invalid_payload(&read_batch(&rle_payload, Table::Spans)));
    }

    #[test]
    fn a_crc_valid_record_with_a_corrupt_frame_is_corrupt_per_skip_corrupt() {
        let good = payload_of(&spans_batch(), 1);
        let mut bad = payload_of(&spans_batch(), 2);
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;

        // Every outer WAL frame has a real checksum, so only `read_batch` rejects seq 2.
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
