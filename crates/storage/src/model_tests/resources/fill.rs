//! Filler rows of project C (R5.1): rows that only take space. The model never reads project C,
//! so a filler append that a restart replays (an unacknowledged WAL group) cannot break a model
//! check.

use std::sync::OnceLock;
use std::time::Duration;

use arrow::array::RecordBatch;
use llmobs_core::ids::ProjectId;
use llmobs_core::span::Span;

use super::super::harness::{Harness, visible_span_rows};
use super::super::shape::{HOUR_NS, SECOND_NS, day, midnight};
use crate::AppendReceipt;
use crate::error::AppendError;
use crate::test_support::{span_row, span_rows};

pub(super) const FILLER_PROJECT: &str = "018f6b4e-7c1a-7c1a-8c1a-0000000000c3";

pub(super) fn filler() -> ProjectId {
    FILLER_PROJECT
        .parse()
        .expect("the filler project id constant parses")
}

/// What a filler span's `input` holds.
#[derive(Debug, Clone, Copy)]
pub(super) enum Text {
    /// Words from a 512-word vocabulary, chosen by a xorshift64 seeded with (batch, span):
    /// compresses like the benchmarks' payloads.
    Words,
    /// A 16-byte prefix unique per (batch, span), then one 64-byte pattern repeated: tiny once
    /// compressed, full size in Arrow (and in spill files).
    Repeated,
}

const VOCABULARY_SIZE: usize = 512;
const VOCABULARY_SEED: u64 = 0x9e37_79b9_7f4a_7c15;
const PATTERN: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_";
const PREFIX_BYTES: usize = 16;

fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// The fixed vocabulary: lowercase words of 3 to 10 letters.
fn vocabulary() -> &'static [String] {
    static WORDS: OnceLock<Vec<String>> = OnceLock::new();
    WORDS.get_or_init(|| {
        let mut state = VOCABULARY_SEED;
        (0..VOCABULARY_SIZE)
            .map(|_| {
                let len = 3 + xorshift(&mut state) % 8;
                (0..len)
                    .map(|_| {
                        char::from(b'a' + u8::try_from(xorshift(&mut state) % 26).unwrap_or(0))
                    })
                    .collect()
            })
            .collect()
    })
}

/// A non-zero xorshift state per (batch, span).
fn seed_of(batch: u16, span: u8) -> u64 {
    ((u64::from(batch) << 8) | u64::from(span)).wrapping_mul(VOCABULARY_SEED) | 1
}

fn words_input(batch: u16, span: u8, input_bytes: usize) -> String {
    let words = vocabulary();
    let mut state = seed_of(batch, span);
    let mut text = String::with_capacity(input_bytes + 16);
    while text.len() < input_bytes {
        let pick = usize::try_from(xorshift(&mut state) % VOCABULARY_SIZE as u64).unwrap_or(0);
        text.push_str(&words[pick]);
        text.push(' ');
    }
    text.truncate(input_bytes);
    text
}

fn repeated_input(batch: u16, span: u8, input_bytes: usize) -> String {
    let prefix = format!("{:016x}", (u64::from(batch) << 8) | u64::from(span));
    let prefix = prefix.as_bytes();
    (0..input_bytes)
        .map(|index| {
            if index < PREFIX_BYTES {
                char::from(prefix[index])
            } else {
                char::from(PATTERN[(index - PREFIX_BYTES) % PATTERN.len()])
            }
        })
        .collect()
}

/// One trace `1000 + batch` of project C: `spans` spans (1..=200) with an `input` of
/// `input_bytes` bytes (at least 16). Span `s` starts at `midnight(day(day_offset)) + 9 h + s
/// seconds`; every row has `ingest_time = ingest`.
pub(super) fn filler_batch(
    batch: u16,
    spans: u8,
    input_bytes: usize,
    text: Text,
    day_offset: u8,
    ingest: i64,
) -> RecordBatch {
    assert!(
        batch < 60_000,
        "a filler batch number must keep trace 1000 + batch in a u16"
    );
    assert!(
        (1..=200).contains(&spans),
        "a filler batch has 1 to 200 spans"
    );
    assert!(
        input_bytes >= PREFIX_BYTES,
        "a filler input holds at least its {PREFIX_BYTES}-byte prefix"
    );
    let trace = 1000 + batch;
    let start = midnight(day(day_offset)) + 9 * HOUR_NS;
    let rows: Vec<Span> = (0..spans)
        .map(|span| {
            let mut row = span_row(trace, span, start + i64::from(span) * SECOND_NS, ingest);
            row.project_id = filler();
            row.name = "filler".to_owned();
            row.input = Some(match text {
                Text::Words => words_input(batch, span, input_bytes),
                Text::Repeated => repeated_input(batch, span, input_bytes),
            });
            row
        })
        .collect();
    span_rows(&rows)
}

/// Rows of project C that a read sees now.
pub(super) fn filler_rows(h: &Harness) -> u64 {
    u64::try_from(visible_span_rows(h, filler())).expect("a row count fits in u64")
}

/// Appends filler batches (`next(i)` builds batch `i`) until `stop` returns true for an outcome
/// or `max_batches` were sent, sleeping `pace` between appends. Returns every outcome in order.
pub(super) fn append_filler(
    h: &Harness,
    mut next: impl FnMut(u16) -> RecordBatch,
    pace: Duration,
    mut stop: impl FnMut(&Result<AppendReceipt, AppendError>) -> bool,
    max_batches: u16,
) -> Vec<Result<AppendReceipt, AppendError>> {
    let mut outcomes = Vec::new();
    for number in 0..max_batches {
        let batch = next(number);
        let outcome = h.block_on(h.storage().append_spans(filler(), batch));
        let done = stop(&outcome);
        outcomes.push(outcome);
        if done {
            break;
        }
        std::thread::sleep(pace);
    }
    outcomes
}
