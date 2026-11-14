//! The oracle: a plain-Rust model of the rows every read must return.
//!
//! It re-implements only the spec rules it needs, over plain structs. It must not call the
//! engine's merge, view, read or query code, or DataFusion: a model that reuses the engine's
//! code checks nothing.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use llmobs_core::enums::{SpanKind, SpanStatus};
use llmobs_core::span::Span;
use uuid::Uuid;

use super::shape::{
    P, ScoreKey, Target, date_of, day, hex, score_created_at, score_id, span, trace_id, trace_shape,
};

/// One stored copy of a span key. Every copy of a key has the same content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SpanCopy {
    pub(super) ingest: i64,
}

/// One stored version of a score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScoreVersion {
    pub(super) version: u32,
    pub(super) deleted: bool,
    pub(super) ingest: i64,
}

/// One project's state.
pub(super) struct ProjectModel {
    /// Which project: `trace_shape` needs it for user, session and environment.
    pub(super) project: P,
    /// Every stored copy of every span key. An empty Vec is never kept.
    pub(super) spans: BTreeMap<(u8, u8), Vec<SpanCopy>>,
    /// Next span number to append, per trace (spans are appended in order).
    pub(super) next_span: BTreeMap<u8, u8>,
    /// Every stored score version, per key.
    pub(super) scores: BTreeMap<ScoreKey, Vec<ScoreVersion>>,
    /// Deletions registered for this project, in order.
    pub(super) deletions: Vec<ModelDeletion>,
    /// Retention watermark: days before it are gone and their later writes are not kept.
    pub(super) keep_from: Option<NaiveDate>,
    pub(super) dropped: bool,
}

/// A registered deletion, as the model sees it.
pub(super) struct ModelDeletion {
    pub(super) id: Uuid,
    pub(super) cutoff: i64,
    pub(super) traces: BTreeSet<u8>,
    /// `User` scope only.
    pub(super) user: Option<String>,
    pub(super) sessions: BTreeSet<String>,
}

pub(super) struct Model {
    pub(super) a: ProjectModel,
    pub(super) b: ProjectModel,
}

/// One expected `traces` view row.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ExpectedTrace {
    pub(super) trace_id: String,
    pub(super) start_time: i64,
    pub(super) end_time: i64,
    pub(super) name: String,
    pub(super) root_span_id: Option<String>,
    pub(super) has_root: bool,
    pub(super) status: &'static str,
    pub(super) session_id: Option<String>,
    pub(super) user_id: Option<String>,
    pub(super) environment: Option<String>,
    pub(super) tags: Vec<String>,
    pub(super) models: Vec<String>,
    pub(super) span_count: i64,
    pub(super) error_count: i64,
    pub(super) llm_count: i64,
    pub(super) total_tokens: Option<i64>,
    pub(super) cost_total: Option<f64>,
}

/// One live score.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ExpectedScore {
    pub(super) score_id: String,
    pub(super) name: String,
    pub(super) target_kind: &'static str,
    pub(super) trace_id: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) value_num: f64,
    pub(super) created_at: i64,
}

impl Model {
    /// `a.project = P::A`, `b.project = P::B`, everything else empty.
    pub(super) fn new() -> Self {
        Self {
            a: ProjectModel::new(P::A),
            b: ProjectModel::new(P::B),
        }
    }

    pub(super) fn project(&self, p: P) -> &ProjectModel {
        match p {
            P::A => &self.a,
            P::B => &self.b,
        }
    }

    pub(super) fn project_mut(&mut self, p: P) -> &mut ProjectModel {
        match p {
            P::A => &mut self.a,
            P::B => &mut self.b,
        }
    }
}

impl ProjectModel {
    pub(super) fn new(project: P) -> Self {
        Self {
            project,
            spans: BTreeMap::new(),
            next_span: BTreeMap::new(),
            scores: BTreeMap::new(),
            deletions: Vec::new(),
            keep_from: None,
            dropped: false,
        }
    }

    /// Whether a row of `day` is kept: not dropped, and not before the retention watermark.
    fn keeps(&self, day: NaiveDate) -> bool {
        !self.dropped && self.keep_from.is_none_or(|keep| day >= keep)
    }

    /// Stores one copy of span `(trace, span_no)` written at `ingest`, unless the keep rule drops it.
    pub(super) fn write_span(&mut self, trace: u8, span_no: u8, ingest: i64) {
        let shape = trace_shape(self.project, trace);
        if self.keeps(day(shape.day_offset)) {
            self.spans
                .entry((trace, span_no))
                .or_default()
                .push(SpanCopy { ingest });
        }
    }

    /// Stores one version of `key` written at `ingest`, unless the keep rule drops it.
    pub(super) fn write_score(&mut self, key: ScoreKey, version: u32, deleted: bool, ingest: i64) {
        debug_assert_eq!(key.project, self.project, "a score of the other project");
        if self.keeps(date_of(score_created_at(&key))) {
            self.scores.entry(key).or_default().push(ScoreVersion {
                version,
                deleted,
                ingest,
            });
        }
    }

    /// Retention: the watermark never moves back. Copies and versions of days before it go.
    pub(super) fn apply_retention(&mut self, keep: NaiveDate) {
        let new_keep = self.keep_from.map_or(keep, |old| old.max(keep));
        self.keep_from = Some(new_keep);
        let project = self.project;
        self.spans
            .retain(|(trace, _), _| day(trace_shape(project, *trace).day_offset) >= new_keep);
        self.scores
            .retain(|key, _| date_of(score_created_at(key)) >= new_keep);
    }

    /// Drop: the project is gone. Its deletions stay, so their status checks go on.
    pub(super) fn drop_project(&mut self) {
        self.dropped = true;
        self.spans.clear();
        self.scores.clear();
    }

    /// Whether a span copy of `trace` written at `ingest` is covered by a deletion (spec 03 §7).
    fn covers_span(&self, trace: u8, ingest: i64) -> bool {
        let user = trace_shape(self.project, trace).user;
        self.deletions.iter().any(|deletion| {
            ingest <= deletion.cutoff
                && (deletion.traces.contains(&trace)
                    || user_matches(deletion.user.as_deref(), user.as_deref()))
        })
    }

    /// Whether a score version written at `ingest` is covered by a deletion (spec 03 §7).
    fn covers_score(&self, key: &ScoreKey, ingest: i64) -> bool {
        let (trace, session, user) = score_context(self.project, key);
        self.deletions.iter().any(|deletion| {
            ingest <= deletion.cutoff
                && (trace.is_some_and(|trace| deletion.traces.contains(&trace))
                    || user_matches(deletion.user.as_deref(), user.as_deref())
                    || session
                        .as_deref()
                        .is_some_and(|session| deletion.sessions.contains(session)))
        })
    }

    /// Visible copies per key: not covered. Keys with no visible copy are absent.
    pub(super) fn visible_spans(&self) -> BTreeMap<(u8, u8), Vec<i64>> {
        let mut visible = BTreeMap::new();
        for (&(trace, span_no), copies) in &self.spans {
            let ingests: Vec<i64> = copies
                .iter()
                .map(|copy| copy.ingest)
                .filter(|&ingest| !self.covers_span(trace, ingest))
                .collect();
            if !ingests.is_empty() {
                visible.insert((trace, span_no), ingests);
            }
        }
        visible
    }

    /// One expected `traces` view row per trace with a visible key, sorted by trace number.
    pub(super) fn expected_traces(&self) -> Vec<ExpectedTrace> {
        let mut by_trace: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
        for &(trace, span_no) in self.visible_spans().keys() {
            by_trace.entry(trace).or_default().push(span_no);
        }
        by_trace
            .into_iter()
            .map(|(trace, span_nos)| self.expected_trace(trace, &span_nos))
            .collect()
    }

    /// The merge of one trace's visible keys, computed from all of its spans at once.
    fn expected_trace(&self, trace: u8, span_nos: &[u8]) -> ExpectedTrace {
        let shape = trace_shape(self.project, trace);
        let rows: Vec<(u8, Span)> = span_nos
            .iter()
            .map(|&span_no| (span_no, span(self.project, trace, span_no, 0)))
            .collect();
        let has_root = shape.has_root && span_nos.contains(&0);
        let root: Option<&Span> = if has_root {
            rows.iter()
                .find(|(span_no, _)| *span_no == 0)
                .map(|(_, row)| row)
        } else {
            None
        };
        let name = match root {
            Some(root) => root.name.clone(),
            None => rows
                .iter()
                .map(|(_, row)| row)
                .min_by_key(|row| (row.start_time, row.span_id))
                .expect("a visible trace has a span")
                .name
                .clone(),
        };
        let any_error = rows.iter().any(|(_, row)| row.status == SpanStatus::Error);
        let status = if any_error {
            "error"
        } else if let Some(root) = root {
            status_name(root.status)
        } else {
            "unset"
        };
        let tags: BTreeSet<String> = rows
            .iter()
            .flat_map(|(_, row)| row.tags.iter().cloned())
            .collect();
        let models: BTreeSet<String> = rows
            .iter()
            .filter_map(|(_, row)| {
                row.response_model
                    .clone()
                    .or_else(|| row.request_model.clone())
            })
            .collect();
        ExpectedTrace {
            trace_id: hex(trace_id(trace).as_bytes()),
            start_time: rows
                .iter()
                .map(|(_, row)| row.start_time)
                .min()
                .expect("a visible trace has a span"),
            end_time: rows
                .iter()
                .map(|(_, row)| row.end_time)
                .max()
                .expect("a visible trace has a span"),
            name,
            root_span_id: root.map(|root| hex(root.span_id.as_bytes())),
            has_root,
            status,
            session_id: shape.session,
            user_id: shape.user,
            environment: shape.environment,
            tags: tags.into_iter().collect(),
            models: models.into_iter().collect(),
            span_count: count(rows.len()),
            error_count: count(
                rows.iter()
                    .filter(|(_, row)| row.status == SpanStatus::Error)
                    .count(),
            ),
            llm_count: count(
                rows.iter()
                    .filter(|(_, row)| row.span_kind == SpanKind::Llm)
                    .count(),
            ),
            total_tokens: sum(rows.iter().map(|(_, row)| row.total_tokens)),
            cost_total: sum(rows.iter().map(|(_, row)| row.cost_total)),
        }
    }

    /// Whether `trace` has some key with two or more visible copies (unmerged re-sends).
    pub(super) fn unsettled(&self, trace: u8) -> bool {
        self.visible_spans()
            .iter()
            .any(|(&(key_trace, _), ingests)| key_trace == trace && ingests.len() >= 2)
    }

    /// Live scores: per key, the visible version with the greatest `(updated_at, ingest)`,
    /// dropped when it is `deleted`.
    pub(super) fn expected_scores(&self) -> Vec<ExpectedScore> {
        let mut live = Vec::new();
        for (key, versions) in &self.scores {
            let created_at = score_created_at(key);
            let newest = versions
                .iter()
                .filter(|version| !self.covers_score(key, version.ingest))
                .max_by_key(|version| (created_at + i64::from(version.version), version.ingest));
            let Some(newest) = newest else { continue };
            if newest.deleted {
                continue;
            }
            let (trace, session, _) = score_context(self.project, key);
            live.push(ExpectedScore {
                score_id: score_id(key).to_string(),
                name: format!("n{}", key.name),
                target_kind: match key.target {
                    Target::Trace(_) => "trace",
                    Target::Session(_) => "session",
                },
                trace_id: trace.map(|trace| hex(trace_id(trace).as_bytes())),
                session_id: session,
                value_num: f64::from(newest.version),
                created_at,
            });
        }
        live
    }
}

/// The trace (if any), session and user a score of `key` carries (spec 01 § Score).
/// A trace score copies its trace's session and user. A session score has no trace or user.
fn score_context(project: P, key: &ScoreKey) -> (Option<u8>, Option<String>, Option<String>) {
    match key.target {
        Target::Trace(trace) => {
            let shape = trace_shape(project, trace);
            (Some(trace), shape.session, shape.user)
        }
        Target::Session(session) => (None, Some(format!("s{session}")), None),
    }
}

/// A user deletion covers a row of the same user. Only `Some` values match: `None == None`
/// must not cover a row without a user.
fn user_matches(deletion: Option<&str>, row: Option<&str>) -> bool {
    match (deletion, row) {
        (Some(deletion), Some(row)) => deletion == row,
        _ => false,
    }
}

fn status_name(status: SpanStatus) -> &'static str {
    match status {
        SpanStatus::Unset => "unset",
        SpanStatus::Ok => "ok",
        SpanStatus::Error => "error",
    }
}

/// Sum of the present values, `None` when none is present.
fn sum<T: std::ops::Add<Output = T>>(values: impl Iterator<Item = Option<T>>) -> Option<T> {
    values.flatten().reduce(|total, value| total + value)
}

fn count(value: usize) -> i64 {
    i64::try_from(value).expect("a trace has fewer spans than i64::MAX")
}

#[cfg(test)]
mod tests {
    use super::super::shape::midnight;
    use super::*;

    /// A project A model with one span copy per key, written at ingest 1.
    fn with_spans(keys: &[(u8, u8)]) -> ProjectModel {
        let mut model = ProjectModel::new(P::A);
        for &(trace, span_no) in keys {
            model.write_span(trace, span_no, 1);
        }
        model
    }

    fn expected_for(model: &ProjectModel, trace: u8) -> ExpectedTrace {
        let id = hex(trace_id(trace).as_bytes());
        model
            .expected_traces()
            .into_iter()
            .find(|row| row.trace_id == id)
            .expect("trace is expected")
    }

    fn traces_deletion(traces: &[u8], cutoff: i64) -> ModelDeletion {
        ModelDeletion {
            id: Uuid::nil(),
            cutoff,
            traces: traces.iter().copied().collect::<BTreeSet<u8>>(),
            user: None,
            sessions: BTreeSet::new(),
        }
    }

    #[test]
    fn expected_trace_without_root_takes_the_earliest_name() {
        // Trace 2 has no root (2 % 3 == 2). Span 1 starts before span 3.
        let model = with_spans(&[(2, 1), (2, 3)]);
        let row = expected_for(&model, 2);
        assert!(!row.has_root);
        assert_eq!(row.root_span_id, None);
        assert_eq!(row.name, "op-1");
        assert_eq!(row.status, "unset");
        assert_eq!(row.span_count, 2);
    }

    #[test]
    fn expected_trace_error_wins_over_root_status() {
        // Trace 1 has a root. Its span 2 is the error (1 % 5 == 1).
        let model = with_spans(&[(1, 0), (1, 2)]);
        let row = expected_for(&model, 1);
        assert!(row.has_root);
        assert_eq!(row.name, "root");
        assert_eq!(row.status, "error");
        assert_eq!(row.error_count, 1);
    }

    #[test]
    fn expected_trace_sums_none_when_no_span_has_tokens() {
        // Spans 0 and 2 are not LLM spans: they have no tokens and no cost.
        let model = with_spans(&[(0, 0), (0, 2)]);
        let row = expected_for(&model, 0);
        assert_eq!(row.total_tokens, None);
        assert_eq!(row.cost_total, None);
        assert_eq!(row.llm_count, 0);
    }

    #[test]
    fn deleted_copies_vanish_and_late_copies_stay() {
        let mut model = ProjectModel::new(P::A);
        model.write_span(1, 0, 5);
        model.write_span(1, 1, 20);
        model.deletions.push(traces_deletion(&[1], 10));
        let row = expected_for(&model, 1);
        assert_eq!(row.span_count, 1);
        assert!(!row.has_root);
        assert_eq!(row.name, "op-1");
        assert_eq!(row.root_span_id, None);
    }

    #[test]
    fn traces_deletion_does_not_cover_rows_without_a_user() {
        // Trace 4 has no user in project A. A Traces deletion has no user, so it must not
        // cover trace 4 just because both users are absent.
        assert!(trace_shape(P::A, 4).user.is_none());
        let mut model = ProjectModel::new(P::A);
        model.write_span(4, 0, 5);
        model.write_span(1, 0, 5);
        model.deletions.push(traces_deletion(&[1], 10));
        let visible = model.visible_spans();
        assert!(visible.contains_key(&(4, 0)));
        assert!(!visible.contains_key(&(1, 0)));
    }

    #[test]
    fn user_deletion_covers_scores_by_session() {
        // Trace 0 has user u0 and session s0 in project A.
        let shape = trace_shape(P::A, 0);
        assert_eq!(shape.user.as_deref(), Some("u0"));
        assert_eq!(shape.session.as_deref(), Some("s0"));

        let mut model = ProjectModel::new(P::A);
        model.deletions.push(ModelDeletion {
            id: Uuid::nil(),
            cutoff: 10,
            traces: BTreeSet::new(),
            user: Some("u0".to_owned()),
            sessions: BTreeSet::from(["s2".to_owned()]),
        });
        let session_two = ScoreKey {
            project: P::A,
            target: Target::Session(2),
            name: 0,
        };
        let session_one = ScoreKey {
            project: P::A,
            target: Target::Session(1),
            name: 0,
        };
        let trace_zero = ScoreKey {
            project: P::A,
            target: Target::Trace(0),
            name: 0,
        };
        for key in [session_two, session_one, trace_zero] {
            model.write_score(key, 1, false, 5);
        }
        let live: Vec<String> = model
            .expected_scores()
            .into_iter()
            .map(|score| score.score_id)
            .collect();
        // Session 2 is covered by its session, trace 0 by its user; session 1 stays.
        assert_eq!(live, vec![score_id(&session_one).to_string()]);
    }

    #[test]
    fn user_deletion_hides_the_users_spans_only() {
        // Trace 0 has user u0 in project A. Trace 4 has no user.
        assert_eq!(trace_shape(P::A, 0).user.as_deref(), Some("u0"));
        assert!(trace_shape(P::A, 4).user.is_none());
        let mut model = ProjectModel::new(P::A);
        model.write_span(0, 0, 5);
        model.write_span(0, 1, 5);
        model.write_span(4, 0, 5);
        model.deletions.push(ModelDeletion {
            id: Uuid::nil(),
            cutoff: 10,
            traces: BTreeSet::new(),
            user: Some("u0".to_owned()),
            sessions: BTreeSet::new(),
        });
        let visible = model.visible_spans();
        assert!(!visible.contains_key(&(0, 0)));
        assert!(!visible.contains_key(&(0, 1)));
        assert!(visible.contains_key(&(4, 0)));
        let traces: Vec<String> = model
            .expected_traces()
            .into_iter()
            .map(|row| row.trace_id)
            .collect();
        assert_eq!(traces, vec![hex(trace_id(4).as_bytes())]);
    }

    #[test]
    fn has_root_is_false_when_span_0_is_covered() {
        // Trace 1 has a root. Only span 0 is covered (ingest 5 <= cutoff 10). Spans 1 and 2
        // were written later and stay.
        assert!(trace_shape(P::A, 1).has_root);
        let mut model = ProjectModel::new(P::A);
        model.write_span(1, 0, 5);
        model.write_span(1, 1, 20);
        model.write_span(1, 2, 20);
        model.deletions.push(traces_deletion(&[1], 10));
        let row = expected_for(&model, 1);
        assert!(!row.has_root);
        assert_eq!(row.root_span_id, None);
        assert_eq!(row.name, "op-1");
        assert_eq!(row.span_count, 2);
    }

    #[test]
    fn live_score_is_the_greatest_updated_at_and_drops_deleted() {
        let key = ScoreKey {
            project: P::A,
            target: Target::Session(0),
            name: 0,
        };
        let mut model = ProjectModel::new(P::A);
        model.write_score(key, 1, false, 1);
        model.write_score(key, 2, true, 2);
        assert!(model.expected_scores().is_empty());

        model.write_score(key, 3, false, 3);
        let live = model.expected_scores();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].value_num, 3.0);
        assert_eq!(live[0].created_at, score_created_at(&key));
    }

    #[test]
    fn live_score_follows_updated_at_not_ingest_order() {
        // Version 3 is written first and version 2 later. updated_at picks version 3,
        // not the later ingest.
        let key = ScoreKey {
            project: P::A,
            target: Target::Session(0),
            name: 1,
        };
        let mut model = ProjectModel::new(P::A);
        model.write_score(key, 3, false, 1);
        model.write_score(key, 2, true, 5);
        let live = model.expected_scores();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].value_num, 3.0);
    }

    #[test]
    fn live_score_tie_on_updated_at_goes_to_the_later_ingest() {
        // The same version twice has the same updated_at. The row with ingest 7 wins.
        let later_live = ScoreKey {
            project: P::A,
            target: Target::Session(1),
            name: 2,
        };
        let later_deleted = ScoreKey {
            project: P::A,
            target: Target::Session(2),
            name: 2,
        };
        let mut model = ProjectModel::new(P::A);
        model.write_score(later_live, 4, true, 1);
        model.write_score(later_live, 4, false, 7);
        model.write_score(later_deleted, 4, false, 1);
        model.write_score(later_deleted, 4, true, 7);

        let live = model.expected_scores();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].score_id, score_id(&later_live).to_string());
        assert_eq!(live[0].value_num, 4.0);
    }

    #[test]
    fn retention_drops_the_day_and_hides_later_writes_to_it() {
        // Trace 0 is on day D, trace 1 on day D + 1.
        let mut model = ProjectModel::new(P::A);
        model.write_span(0, 0, 1);
        model.write_span(1, 0, 1);
        model.apply_retention(day(1));
        assert_eq!(
            model.visible_spans().keys().copied().collect::<Vec<_>>(),
            vec![(1, 0)]
        );

        // Day D is before the watermark: the write is not kept. Day D + 1 is kept.
        model.write_span(0, 1, 2);
        model.write_span(1, 1, 2);
        assert_eq!(
            model.visible_spans().keys().copied().collect::<Vec<_>>(),
            vec![(1, 0), (1, 1)]
        );

        // The watermark never moves back.
        model.apply_retention(day(0));
        assert_eq!(model.keep_from, Some(day(1)));
    }

    #[test]
    fn midnight_and_date_of_are_inverse_at_day_starts() {
        assert_eq!(date_of(midnight(day(2))), day(2));
        assert_eq!(date_of(midnight(day(2)) - 1), day(1));
    }
}
