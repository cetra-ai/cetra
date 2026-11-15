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

/// The two projects. Both parse as `ProjectId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum P {
    A,
    B,
}

impl P {
    pub(super) fn id(self) -> ProjectId {
        let raw = match self {
            Self::A => PROJECT_A,
            Self::B => PROJECT_B,
        };
        raw.parse().expect("project id constant parses")
    }

    /// 0 for A, 1 for B. Mixed into the content so the projects differ.
    pub(super) fn salt(self) -> u8 {
        match self {
            Self::A => 0,
            Self::B => 1,
        }
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
