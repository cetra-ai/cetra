//! What every row of the model test looks like (docs/plans/r-hardening/01-m1-model.md § Shapes).
//!
//! Everything about a trace's content is a pure function of (project, trace number, span
//! number). Ops only choose which rows to write and when. This module builds rows with
//! `test_support::{span_row, score_row}` and does not call the engine's merge or read code.

use std::collections::BTreeMap;

use chrono::{Datelike, Days, NaiveDate};
use llmobs_core::enums::{SpanKind, SpanStatus};
use llmobs_core::ids::{ProjectId, SpanId, TraceId};
use llmobs_core::names::{environment, metadata_key};
use llmobs_core::score::{Score, ScoreTarget, ScoreValue};
use llmobs_core::span::Span;
use uuid::Uuid;

use super::ops::TRACES;
use crate::test_support::{score_row, span_row};

pub(super) const PROJECT_A: &str = "018f6b4e-7c1a-7c1a-8c1a-0000000000a1";
pub(super) const PROJECT_B: &str = "018f6b4e-7c1a-7c1a-8c1a-0000000000b2";

pub(super) const DAY_NS: i64 = 86_400_000_000_000;
pub(super) const HOUR_NS: i64 = 3_600_000_000_000;
pub(super) const MINUTE_NS: i64 = 60_000_000_000;
pub(super) const SECOND_NS: i64 = 1_000_000_000;

/// Day `D` (2026-03-05, UTC) plus `offset` days.
pub(super) fn day(offset: u8) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 3, 5)
        .and_then(|date| date.checked_add_days(Days::new(u64::from(offset))))
        .expect("day D plus a small offset is a valid date")
}

/// Nanoseconds since the Unix epoch of 00:00 UTC on `date`.
pub(super) fn midnight(date: NaiveDate) -> i64 {
    (i64::from(date.num_days_from_ce()) - 719_163) * DAY_NS
}

/// The UTC date that contains the instant `ns`.
pub(super) fn date_of(ns: i64) -> NaiveDate {
    let days =
        i32::try_from(ns.div_euclid(DAY_NS) + 719_163).expect("timestamp within chrono's range");
    NaiveDate::from_num_days_from_ce_opt(days).expect("timestamp within chrono's range")
}

/// A project of the test, numbered: lane `n` of the concurrent mode uses `P(2n)` and `P(2n + 1)`
/// (`P::A` and `P::B` are lane 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct P(pub(super) u8);

impl P {
    pub(super) const A: P = P(0);
    pub(super) const B: P = P(1);

    /// The two projects of lane `lane`: `P(2 * lane)` and `P(2 * lane + 1)` (R6.7). At most 15.
    pub(super) fn pair(lane: u8) -> (P, P) {
        debug_assert!(lane < 16, "lane {lane} has no project pair under 32");
        (P(2 * lane), P(2 * lane + 1))
    }

    /// `PROJECT_A` for 0, `PROJECT_B` for 1, else a UUID of the same shape with the number in
    /// its last field.
    pub(super) fn id(self) -> ProjectId {
        let raw = match self.0 {
            0 => PROJECT_A.to_owned(),
            1 => PROJECT_B.to_owned(),
            n => format!("018f6b4e-7c1a-7c1a-8c1a-{:012x}", 0xc00 + u64::from(n)),
        };
        raw.parse().expect("project id constant parses")
    }

    /// The project's number. Mixed into the content so the projects differ; at most 31 (16
    /// lanes), which the score id's 8-bit salt field holds.
    pub(super) fn salt(self) -> u8 {
        debug_assert!(
            self.0 < 32,
            "project {} does not fit the 5-bit salt",
            self.0
        );
        self.0
    }
}

/// Per-trace constants.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct TraceShape {
    pub(super) trace: u8,
    /// The trace's partition day for retention: `D + day_offset`.
    pub(super) day_offset: u8,
    /// Start of span 0.
    pub(super) base: i64,
    pub(super) has_root: bool,
    pub(super) user: Option<String>,
    pub(super) session: Option<String>,
    pub(super) environment: Option<String>,
}

pub(super) fn trace_shape(project: P, trace: u8) -> TraceShape {
    let k = u32::from(trace) + u32::from(project.salt());
    let (day_offset, base) = match trace % 4 {
        0 => (0, midnight(day(0)) + 10 * HOUR_NS),
        1 => (1, midnight(day(1)) + 10 * HOUR_NS),
        // Crosses midnight: the spans start before D + 2 and the partition day is D + 1.
        2 => (1, midnight(day(2)) - 10 * MINUTE_NS),
        _ => (3, midnight(day(3)) + 10 * HOUR_NS),
    };
    let user = [Some("u0"), Some("u1"), None][((k / 2) % 3) as usize].map(str::to_owned);
    let session = (k % 5 != 4).then(|| format!("s{}", k % 4));
    let env = [None, Some("production"), Some(environment::EVAL)][((k / 3) % 3) as usize]
        .map(str::to_owned);
    TraceShape {
        trace,
        day_offset,
        base,
        has_root: trace % 3 != 2,
        user,
        session,
        environment: env,
    }
}

/// The id of the deletion a project's op registers at `clock` (R2.11). A UUIDv7-shaped value with a
/// fixed timestamp, the project's salt in byte 7 and the clock in the last eight bytes, so a parent
/// that replays the same ops registers the same ids. Lanes share one harness and start their clocks
/// at the same value, so the salt keeps their ids apart (`Deletion.id` is a global catalog key).
/// `ops::apply` raises the clock by at least a microsecond per op, so the ids of one project differ.
/// The top two bits of the clock are zero, so the variant bits do not collide.
pub(super) fn deletion_id(project: P, clock: i64) -> Uuid {
    debug_assert!(clock >= 0, "a deletion id needs a non-negative clock");
    const FIXED_MILLIS: u64 = 1_700_000_000_000;
    let mut random = [0_u8; 10];
    random[1] = project.salt();
    random[2..].copy_from_slice(&clock.to_be_bytes());
    uuid::Builder::from_unix_timestamp_millis(FIXED_MILLIS, &random).into_uuid()
}

/// The trace id `span_row` builds for `trace`: the 2 big-endian bytes of `trace + 1`, repeated.
pub(super) fn trace_id(trace: u8) -> TraceId {
    let pair = (u16::from(trace) + 1).to_be_bytes();
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = pair[index % 2];
    }
    TraceId::from_bytes(bytes).expect("trace id is never all zero")
}

/// The trace an evaluation trace of `trace` targets: `(trace + 1) % TRACES`.
pub(super) fn eval_target(trace: u8) -> u8 {
    (trace + 1) % TRACES
}

/// The span id `span_row` builds for `span`: 8 bytes of `span + 1`.
pub(super) fn span_id(span: u8) -> SpanId {
    SpanId::from_bytes([span + 1; 8]).expect("span id is never all zero")
}

/// Lowercase hex of `bytes`.
pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The full span `span_no` of `(project, trace)`, stored with `ingest_time = ingest`.
pub(super) fn span(project: P, trace: u8, span_no: u8, ingest: i64) -> Span {
    let shape = trace_shape(project, trace);
    let start = shape.base + i64::from(span_no) * MINUTE_NS;
    let end = start + 30 * SECOND_NS + i64::from(span_no) * SECOND_NS;
    let mut row = span_row(u16::from(trace), span_no, start, ingest);
    row.project_id = project.id();
    row.start_time = start;
    row.end_time = end;
    row.parent_span_id = if span_no != 0 {
        Some(span_id(0))
    } else if shape.has_root {
        None
    } else {
        // A parent that is never stored, so span 0 is not a root and the trace has none.
        Some(SpanId::from_bytes([0xEE; 8]).expect("parent id is never all zero"))
    };
    row.name = if span_no == 0 && shape.has_root {
        "root".to_owned()
    } else {
        format!("op-{span_no}")
    };
    let llm = !span_no.is_multiple_of(2);
    row.span_kind = if llm { SpanKind::Llm } else { SpanKind::Span };
    if llm {
        let model = ["m1", "m2"][(usize::from(span_no / 2) + usize::from(project.salt())) % 2];
        row.response_model = Some(model.to_owned());
        row.total_tokens = Some(10 * i64::from(span_no) + i64::from(project.salt()));
        row.cost_total = Some(0.125 * f64::from(span_no));
    }
    row.status = if trace % 5 == 1 && span_no == 2 {
        SpanStatus::Error
    } else {
        SpanStatus::Ok
    };
    row.tags = vec![
        if span_no.is_multiple_of(2) {
            "even"
        } else {
            "odd"
        }
        .to_owned(),
    ];
    if shape.environment.as_deref() == Some(environment::EVAL) {
        // Every span of an evaluation trace names its target (`eval_trace_ids` reads it).
        row.metadata = BTreeMap::from([(
            metadata_key::EVAL_TARGET_TRACE_ID.to_owned(),
            hex(trace_id(eval_target(trace)).as_bytes()),
        )]);
    }
    row.session_id = shape.session;
    row.user_id = shape.user;
    row.environment = shape.environment;
    row
}

/// A score's identity: its project, what it is attached to, and its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) struct ScoreKey {
    pub(super) project: P,
    pub(super) target: Target,
    pub(super) name: u8,
}

/// What a model score is attached to. Named apart from `llmobs_core::score::ScoreTarget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Target {
    Trace(u8),
    Session(u8),
}

/// The score id of `key`: a packed (salt, kind, target, name), with a marker bit so it is never nil.
pub(super) fn score_id(key: &ScoreKey) -> Uuid {
    let (kind, target) = match key.target {
        Target::Trace(trace) => (0_u128, u128::from(trace)),
        Target::Session(session) => (1_u128, u128::from(session)),
    };
    let packed = (1_u128 << 120)
        | (u128::from(key.project.salt()) << 96)
        | (kind << 88)
        | (target << 16)
        | u128::from(key.name);
    Uuid::from_u128(packed)
}

/// `created_at` of every version of `key`. A trace score is created at its trace's start.
pub(super) fn score_created_at(key: &ScoreKey) -> i64 {
    match key.target {
        Target::Trace(trace) => trace_shape(key.project, trace).base,
        Target::Session(_) => midnight(day(3)) + 11 * HOUR_NS,
    }
}

/// One version of `key`. `version` starts at 1; `updated_at = created_at + version`.
pub(super) fn score(key: &ScoreKey, version: u32, deleted: bool, ingest: i64) -> Score {
    let created_at = score_created_at(key);
    let (trace, session_id, user_id, target) = match key.target {
        Target::Trace(trace) => {
            let shape = trace_shape(key.project, trace);
            (
                u16::from(trace),
                shape.session,
                shape.user,
                ScoreTarget::Trace {
                    trace_id: trace_id(trace),
                },
            )
        }
        Target::Session(session) => {
            let session_id = format!("s{session}");
            (
                0,
                Some(session_id.clone()),
                None,
                ScoreTarget::Session { session_id },
            )
        }
    };
    let mut row = score_row(
        0,
        trace,
        created_at,
        created_at + i64::from(version),
        ingest,
        deleted,
        &format!("n{}", key.name),
    );
    row.project_id = key.project.id();
    row.score_id = score_id(key);
    row.target = target;
    row.session_id = session_id;
    row.user_id = user_id;
    row.value = ScoreValue::Numeric(f64::from(version));
    row
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    /// The id before the project salt existed: the clock in the random bits, byte 7 zero.
    fn deletion_id_without_salt(clock: i64) -> Uuid {
        const FIXED_MILLIS: u64 = 1_700_000_000_000;
        let mut random = [0_u8; 10];
        random[2..].copy_from_slice(&clock.to_be_bytes());
        uuid::Builder::from_unix_timestamp_millis(FIXED_MILLIS, &random).into_uuid()
    }

    #[test]
    fn deletion_ids_of_lanes_at_one_clock_differ() {
        for clock in [0, 1, 1_000, 12 * HOUR_NS] {
            assert_ne!(
                deletion_id(P(0), clock),
                deletion_id(P(2), clock),
                "lane 0 and lane 1 register distinct deletion ids at clock {clock}"
            );
            assert_ne!(deletion_id(P(2), clock), deletion_id(P(3), clock));
            assert_eq!(
                deletion_id(P::A, clock),
                deletion_id_without_salt(clock),
                "project A's deletion ids are unchanged at clock {clock}"
            );
        }
        assert_eq!(
            deletion_id(P(2), 7),
            deletion_id(P(2), 7),
            "a fixed input gives a fixed id"
        );
    }

    #[test]
    fn p_ids_are_distinct_and_a_b_unchanged() {
        assert_eq!(
            P::A.id(),
            PROJECT_A.parse::<ProjectId>().expect("PROJECT_A parses")
        );
        assert_eq!(
            P::B.id(),
            PROJECT_B.parse::<ProjectId>().expect("PROJECT_B parses")
        );
        let ids: HashSet<ProjectId> = (0..32).map(|n| P(n).id()).collect();
        assert_eq!(
            ids.len(),
            32,
            "the 32 projects of 16 lanes have distinct ids"
        );
        assert!(
            (0..32).all(|n| P(n).salt() == n),
            "the salt is the project number"
        );
    }
}
