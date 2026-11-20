//! Issue triggers (D-IC-4): the `score` and `feedback` sources' rules in memory, and the
//! candidates that a written score queues.

use std::collections::HashMap;
use std::sync::{PoisonError, RwLock};

use cetra_core::enums::ScoreSource;
use cetra_core::ids::{ProjectId, SpanId, TraceId};
use cetra_core::score::{Score, ScoreTarget};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::bad_score;
use crate::eval::budget::day_start;
use crate::eval::sampling;
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::issues::{
    CandidateRow, CandidateStatus, CandidateTarget, SourceKind, SourceRow, admit_daily,
    insert_candidate, llm_sources,
};
use crate::meta::projects::ProjectRow;
use crate::meta::score_definitions::DefinitionRow;
use crate::scores::Writer;

/// One `score` or `feedback` source, as the score write reads it.
#[derive(Debug, Clone)]
pub(crate) struct ScoreRule {
    pub(crate) source_id: Uuid,
    pub(crate) kind: SourceKind,
    /// Score names the source reads. Empty matches nothing.
    pub(crate) score_names: Vec<String>,
    /// Feedback writers a `feedback` source reads: `end_user` and `human` only.
    pub(crate) feedback_sources: Vec<ScoreSource>,
    pub(crate) sample_rate: f64,
    pub(crate) daily_cap: u32,
}

/// The score rules of every project, rebuilt by [`refresh_score_rules`] (A11).
#[derive(Debug, Default)]
pub(crate) struct ScoreRules {
    map: RwLock<HashMap<ProjectId, Vec<ScoreRule>>>,
}

impl ScoreRules {
    pub(crate) fn set(&self, map: HashMap<ProjectId, Vec<ScoreRule>>) {
        *self.map.write().unwrap_or_else(PoisonError::into_inner) = map;
    }

    pub(crate) fn of(&self, project: ProjectId) -> Vec<ScoreRule> {
        self.map
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&project)
            .cloned()
            .unwrap_or_default()
    }
}

/// Where a candidate's trace or session is, as the score or the trace trigger found it.
#[derive(Debug, Clone)]
pub(crate) enum Place {
    /// A trace, with the session it belongs to when known.
    Trace {
        trace: TraceId,
        session: Option<String>,
    },
    /// A span of a trace, with the session it belongs to when known.
    Span {
        trace: TraceId,
        span: SpanId,
        session: Option<String>,
    },
    /// A session. The worker resolves it to a trace (A4).
    Session(String),
}

/// A candidate queued at `now` for `source`, with its dedup key (`<source>:<trace>`,
/// `<source>:<trace>:<span>` or `<source>:session:<session>`). The worker fills the rest (A4).
pub(crate) fn queued(
    now: i64,
    project: ProjectId,
    source: Uuid,
    place: Place,
    user_id: Option<String>,
    trace_start: Option<i64>,
    score_id: Option<Uuid>,
) -> CandidateRow {
    let (target, trace_id, span_id, session_id, dedup_key) = match place {
        Place::Trace { trace, session } => (
            CandidateTarget::Trace,
            Some(trace),
            None,
            session,
            format!("{source}:{trace}"),
        ),
        Place::Span {
            trace,
            span,
            session,
        } => (
            CandidateTarget::Span,
            Some(trace),
            Some(span),
            session,
            format!("{source}:{trace}:{span}"),
        ),
        Place::Session(session) => (
            CandidateTarget::Session,
            None,
            None,
            Some(session.clone()),
            format!("{source}:session:{session}"),
        ),
    };
    CandidateRow {
        id: Uuid::now_v7(),
        project_id: project,
        source_id: source,
        target,
        trace_id,
        span_id,
        session_id,
        user_id,
        trace_start,
        score_id,
        dedup_key,
        status: CandidateStatus::Queued,
        attempts: 0,
        claims: 0,
        interruptions: 0,
        load_retries: 0,
        not_before: now,
        error_class: None,
        issue_id: None,
        summary: None,
        summary_prompt_version: None,
        vector: None,
        embed_model: None,
        dimension: None,
        call_trace_id: None,
        cost_usd: 0.0,
        created_at: now,
        updated_at: now,
    }
}

/// Rebuilds the score rules from the enabled `score` and `feedback` sources (A11). The source
/// routes call it after every create, update and delete, and the sweeper calls it on each tick.
///
/// # Errors
///
/// [`MetaError`] when the sources cannot be read; the rules then stay as they were.
pub(crate) async fn refresh_score_rules(state: &AppState) -> Result<(), MetaError> {
    let found = llm_sources(state.meta.reader()).await?;
    let mut map: HashMap<ProjectId, Vec<ScoreRule>> = HashMap::new();
    for (source, _, _) in &found {
        if let Some(rule) = score_rule(source) {
            map.entry(source.project_id).or_default().push(rule);
        }
    }
    state.issue_score_rules.set(map);
    Ok(())
}

/// The rule of a `score` or `feedback` source. `None` for other kinds, and for a source without
/// the sample rate or daily cap its kind requires (D-IC-1).
fn score_rule(source: &SourceRow) -> Option<ScoreRule> {
    let feedback_sources = match source.kind {
        SourceKind::Score => Vec::new(),
        SourceKind::Feedback => source
            .feedback_sources
            .iter()
            .filter_map(|name| ScoreSource::parse(name).ok())
            .filter(|writer| matches!(writer, ScoreSource::EndUser | ScoreSource::Human))
            .collect(),
        SourceKind::Error | SourceKind::Topic => return None,
    };
    Some(ScoreRule {
        source_id: source.id,
        kind: source.kind,
        score_names: source.score_names.clone(),
        feedback_sources,
        sample_rate: source.sample_rate?,
        daily_cap: source.daily_cap?,
    })
}

/// Queues a candidate for each bad score of `rows` that a rule reads and the sampling keeps
/// (D-IC-4). Runs at the end of `scores::write` and never fails the write: a failure is logged.
pub(crate) async fn on_scores_written(
    state: &AppState,
    project: &ProjectRow,
    writer: Writer,
    rows: &[Score],
    definitions: &[DefinitionRow],
) {
    // An issue's own occurrences never trigger a candidate.
    if matches!(writer, Writer::Issue { .. }) {
        return;
    }
    let rules = state.issue_score_rules.of(project.id);
    if rules.is_empty() {
        return;
    }
    let now = state.clock.now_ns();
    // Each entry: the source, its daily cap, and the candidate.
    let mut found: Vec<(Uuid, u32, CandidateRow)> = Vec::new();
    for score in rows.iter().filter(|score| !score.deleted) {
        let definition = definitions
            .iter()
            .find(|def| def.name == score.name && def.archived_at.is_none());
        if !bad_score::is_bad(&score.value, definition) {
            continue;
        }
        let Some((place, sample_trace)) = place_of(score) else {
            continue;
        };
        for rule in rules.iter().filter(|rule| reads(rule, score)) {
            if !sampling::sampled(rule.sample_rate, rule.source_id, sample_trace, None) {
                continue;
            }
            let row = queued(
                now,
                project.id,
                rule.source_id,
                place.clone(),
                score.user_id.clone(),
                None,
                Some(score.score_id),
            );
            found.push((rule.source_id, rule.daily_cap, row));
        }
    }
    if found.is_empty() {
        return;
    }
    let count = found.len();
    let project_id = project.id;
    let outcome = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                let mut inserted = 0;
                for (source, cap, row) in found {
                    if !admit_daily(conn, source, day_start(now), cap).await? {
                        continue;
                    }
                    if insert_candidate(conn, &row).await? {
                        inserted += 1;
                    }
                }
                Ok(inserted)
            })
        })
        .await;
    match outcome {
        Ok(inserted) => {
            tracing::debug!(
                project_id = %project_id,
                found = count,
                inserted,
                "issue candidates queued from scores"
            );
            if inserted > 0 {
                state.issues_wake();
            }
        }
        Err(error) => {
            tracing::warn!(
                project_id = %project_id,
                count,
                error = %error,
                "issue candidates could not be queued from scores"
            );
        }
    }
}

/// Whether `rule` reads `score`: its name is one of the rule's names, the writer is not an issue,
/// and a `feedback` rule reads only its feedback writers (D-IC-1).
fn reads(rule: &ScoreRule, score: &Score) -> bool {
    score.source != ScoreSource::Issue
        && rule.score_names.contains(&score.name)
        && (rule.kind != SourceKind::Feedback || rule.feedback_sources.contains(&score.source))
}

/// The place of a score's target, and the trace whose sampling draw decides it. A session's
/// draw uses the first 16 bytes of SHA-256 of its id (D-IC-4).
fn place_of(score: &Score) -> Option<(Place, TraceId)> {
    match &score.target {
        ScoreTarget::Span { trace_id, span_id } => Some((
            Place::Span {
                trace: *trace_id,
                span: *span_id,
                session: score.session_id.clone(),
            },
            *trace_id,
        )),
        ScoreTarget::Trace { trace_id } => Some((
            Place::Trace {
                trace: *trace_id,
                session: score.session_id.clone(),
            },
            *trace_id,
        )),
        ScoreTarget::Session { session_id } => {
            let digest = Sha256::digest(session_id.as_bytes());
            let trace = TraceId::from_slice(&digest[..16]).ok()?;
            Some((Place::Session(session_id.clone()), trace))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use cetra_core::score::{ScoreTarget, ScoreValue};

    use super::*;
    use crate::api::evaluators::EvaluatorKind;
    use crate::api::scores::ScoreCreate;
    use crate::eval::sampling;
    use crate::meta::issues::insert_source;
    use crate::scores;
    use crate::test_support::TestApp;

    fn trace(n: u8) -> TraceId {
        TraceId::from_slice(&[n; 16]).expect("not all zero")
    }

    /// A judge's writer: its scores are `llm_judge`.
    fn judge() -> Writer {
        Writer::Evaluator {
            kind: EvaluatorKind::LlmJudge,
            evaluator_id: Uuid::now_v7(),
            rule_id: None,
        }
    }

    fn item(target: ScoreTarget, name: &str, value: ScoreValue) -> ScoreCreate {
        ScoreCreate {
            score_id: None,
            target,
            session_id: None,
            user_id: None,
            name: name.to_owned(),
            value,
            reason: None,
            source: None,
            metadata: BTreeMap::new(),
            experiment_id: None,
            dataset_item_id: None,
            queue_id: None,
        }
    }

    /// An enabled source of `kind` on `project`, stored and then loaded into the rules.
    async fn source(
        app: &TestApp,
        project: ProjectId,
        kind: SourceKind,
        names: &[&str],
        feedback: &[&str],
        cap: u32,
    ) -> SourceRow {
        let now = app.state.clock.now_ns();
        let row = SourceRow {
            id: Uuid::now_v7(),
            project_id: project,
            name: format!("Source {}", Uuid::now_v7()),
            kind,
            filter: None,
            enabled: true,
            paused_reason: None,
            paused_at: None,
            created_by: None,
            created_at: now,
            updated_at: now,
            sample_rate: Some(1.0),
            daily_cap: Some(cap),
            score_names: names.iter().map(|name| (*name).to_owned()).collect(),
            feedback_sources: feedback.iter().map(|name| (*name).to_owned()).collect(),
            chat_connection_id: None,
            chat_model: None,
            embed_connection_id: None,
            embed_model: None,
            embed_dimension: None,
            floor: None,
            min_issue_size: None,
            budget_usd: None,
            budget_period: None,
            paused_detail: None,
            next_refit_at: None,
        };
        let stored = row.clone();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_source(conn, &row).await }))
            .await
            .expect("source stored");
        refresh_score_rules(&app.state).await.expect("rules loaded");
        stored
    }

    async fn write(
        app: &TestApp,
        project: &ProjectRow,
        writer: Writer,
        items: Vec<ScoreCreate>,
    ) -> Vec<Uuid> {
        scores::write(&app.state, project, writer, items)
            .await
            .expect("scores written")
    }

    /// The dedup keys of the candidates of `source`, sorted.
    async fn candidates(app: &TestApp, source: Uuid) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT dedup_key FROM issue_candidates WHERE source_id = ? ORDER BY dedup_key",
        )
        .bind(source.to_string())
        .fetch_all(app.state.meta.reader())
        .await
        .expect("candidates read")
    }

    #[tokio::test]
    async fn a_bad_judge_score_queues_one_candidate() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let source = source(
            &app,
            seeded.project.id,
            SourceKind::Score,
            &["quality"],
            &[],
            100,
        )
        .await;
        let writer = judge();
        let t = trace(1);
        let ids = write(
            &app,
            &seeded.project,
            writer,
            vec![item(
                ScoreTarget::Trace { trace_id: t },
                "quality",
                ScoreValue::Boolean(false),
            )],
        )
        .await;
        assert_eq!(
            candidates(&app, source.id).await,
            vec![format!("{}:{t}", source.id)]
        );
        let score_ids: Vec<Option<String>> =
            sqlx::query_scalar("SELECT score_id FROM issue_candidates WHERE source_id = ?")
                .bind(source.id.to_string())
                .fetch_all(app.state.meta.reader())
                .await
                .expect("score ids read");
        assert_eq!(score_ids, vec![Some(ids[0].to_string())]);
    }

    #[tokio::test]
    async fn a_good_score_queues_none() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let source = source(
            &app,
            seeded.project.id,
            SourceKind::Score,
            &["quality"],
            &[],
            100,
        )
        .await;
        write(
            &app,
            &seeded.project,
            judge(),
            vec![item(
                ScoreTarget::Trace { trace_id: trace(1) },
                "quality",
                ScoreValue::Boolean(true),
            )],
        )
        .await;
        assert!(candidates(&app, source.id).await.is_empty());
    }

    #[tokio::test]
    async fn an_issue_occurrence_queues_none() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let source = source(
            &app,
            seeded.project.id,
            SourceKind::Score,
            &["quality"],
            &[],
            100,
        )
        .await;
        let name = format!("issue:{}", source.id);
        // The rule reads the name, so only the writer keeps the occurrence from triggering.
        write(
            &app,
            &seeded.project,
            Writer::Issue {
                source_id: source.id,
            },
            vec![item(
                ScoreTarget::Trace { trace_id: trace(1) },
                &name,
                ScoreValue::Boolean(false),
            )],
        )
        .await;
        assert!(candidates(&app, source.id).await.is_empty());
    }

    #[tokio::test]
    async fn feedback_from_end_users_queues_and_the_api_does_not() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let source = source(
            &app,
            project,
            SourceKind::Feedback,
            &["helpful"],
            &["end_user", "human"],
            100,
        )
        .await;
        let end_user = trace(1);
        write(
            &app,
            &seeded.project,
            Writer::EndUser {
                key_id: Uuid::now_v7(),
            },
            vec![item(
                ScoreTarget::Trace { trace_id: end_user },
                "helpful",
                ScoreValue::Boolean(false),
            )],
        )
        .await;
        write(
            &app,
            &seeded.project,
            Writer::Api {
                author: Uuid::now_v7(),
            },
            vec![item(
                ScoreTarget::Trace { trace_id: trace(2) },
                "helpful",
                ScoreValue::Boolean(false),
            )],
        )
        .await;
        assert_eq!(
            candidates(&app, source.id).await,
            vec![format!("{}:{end_user}", source.id)]
        );
    }

    #[tokio::test]
    async fn the_daily_cap_stops_the_candidates_at_the_cap() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let source = source(
            &app,
            seeded.project.id,
            SourceKind::Score,
            &["quality"],
            &[],
            2,
        )
        .await;
        let items = (1..=3)
            .map(|n| {
                item(
                    ScoreTarget::Trace { trace_id: trace(n) },
                    "quality",
                    ScoreValue::Boolean(false),
                )
            })
            .collect();
        write(&app, &seeded.project, judge(), items).await;
        assert_eq!(candidates(&app, source.id).await.len(), 2);
    }

    #[tokio::test]
    async fn sampling_keeps_the_targets_that_sampled_keeps() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let rate = 0.0001;
        let row = source(
            &app,
            seeded.project.id,
            SourceKind::Score,
            &["quality"],
            &[],
            1000,
        )
        .await;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE issue_sources SET sample_rate = ? WHERE id = ?")
                        .bind(rate)
                        .bind(row.id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .expect("rate stored");
        refresh_score_rules(&app.state).await.expect("rules loaded");
        let mut expected: Vec<String> = (1..=20u8)
            .map(trace)
            .filter(|t| sampling::sampled(rate, row.id, *t, None))
            .map(|t| format!("{}:{t}", row.id))
            .collect();
        expected.sort();
        let items = (1..=20u8)
            .map(|n| {
                item(
                    ScoreTarget::Trace { trace_id: trace(n) },
                    "quality",
                    ScoreValue::Boolean(false),
                )
            })
            .collect();
        write(&app, &seeded.project, judge(), items).await;
        assert_eq!(candidates(&app, row.id).await, expected);
    }

    #[tokio::test]
    async fn the_same_score_written_twice_queues_one_candidate() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let source = source(
            &app,
            seeded.project.id,
            SourceKind::Score,
            &["quality"],
            &[],
            100,
        )
        .await;
        let writer = judge();
        let bad = item(
            ScoreTarget::Trace { trace_id: trace(1) },
            "quality",
            ScoreValue::Boolean(false),
        );
        write(&app, &seeded.project, writer, vec![bad.clone()]).await;
        write(&app, &seeded.project, writer, vec![bad]).await;
        assert_eq!(candidates(&app, source.id).await.len(), 1);
    }
}
