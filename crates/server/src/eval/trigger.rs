//! The online-rule trigger (D-I-7 to D-I-11): the tracked projects, the flusher and startup.
//! I4.7 adds the sweeper.

use std::collections::HashMap;

use llmobs_core::ids::ProjectId;

use crate::eval::pending::{Changes, Entry};
use crate::http::AppState;
use crate::meta::MetaError;
use crate::meta::online_rules::{self, TriggerRule};
use crate::meta::pending_traces::{PendingRow, load_pending, write_pending};
use crate::meta::projects::{
    COMPLETION_TIMEOUT_DEFAULT_S, COMPLETION_TIMEOUT_MAX_S, COMPLETION_TIMEOUT_MIN_S,
};

/// Most `pending_traces` rows written per transaction (D-I-8).
const FLUSH_CHUNK: usize = 1_000;
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// The enabled, unpaused rules of one tracked project, and its completion timeout.
// I4.7 reads it.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectRules {
    pub(crate) timeout_ns: i64,
    pub(crate) rules: Vec<TriggerRule>,
}

/// What one flush wrote.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FlushReport {
    pub(crate) upserts: usize,
    pub(crate) deletes: usize,
}

/// Reads the enabled, unpaused rules, groups them by project, and sets the tracked set to their
/// projects (D-I-8). A project's timeout is its stored `trace_completion_timeout_s` when that is
/// in range, else the default.
///
/// # Errors
///
/// [`MetaError`] when the read fails; the tracked set is then left as it was.
pub(crate) async fn refresh_rules(
    state: &AppState,
) -> Result<HashMap<ProjectId, ProjectRules>, MetaError> {
    let rules = online_rules::trigger_rules(state.meta.reader()).await?;
    let mut projects: HashMap<ProjectId, ProjectRules> = HashMap::new();
    for rule in rules {
        let timeout_ns = timeout_ns(rule.completion_timeout_s);
        projects
            .entry(rule.project_id)
            .or_insert_with(|| ProjectRules {
                timeout_ns,
                rules: Vec::new(),
            })
            .rules
            .push(rule);
    }
    state
        .pending
        .set_tracked(projects.keys().copied().collect());
    Ok(projects)
}

/// Startup: [`refresh_rules`], then the table's rows into the map (D-I-8).
///
/// # Errors
///
/// [`MetaError`] when either read fails.
pub(crate) async fn load(state: &AppState) -> Result<(), MetaError> {
    refresh_rules(state).await?;
    let rows = load_pending(state.meta.reader()).await?;
    state.pending.load(rows.into_iter().map(|row| {
        (
            row.project_id,
            row.trace_id,
            Entry {
                first_seen: row.first_seen,
                last_seen: row.last_seen,
                min_start: row.min_start,
                root_ended: row.root_ended,
                failed_reads: 0,
                spans: 0,
            },
        )
    }));
    Ok(())
}

/// Writes the map's changes to `pending_traces` (D-I-8). A failed write puts back its own
/// changes and every later part's.
///
/// # Errors
///
/// [`MetaError`] when a write fails.
pub(crate) async fn flush_once(state: &AppState) -> Result<FlushReport, MetaError> {
    let mut parts = flush_parts(state.pending.take_changes(), FLUSH_CHUNK).into_iter();
    let mut report = FlushReport::default();
    while let Some(part) = parts.next() {
        let deletes = part.deletes.clone();
        let upserts: Vec<PendingRow> = part
            .upserts
            .iter()
            .map(|(project_id, trace_id, entry)| PendingRow {
                project_id: *project_id,
                trace_id: *trace_id,
                first_seen: entry.first_seen,
                last_seen: entry.last_seen,
                min_start: entry.min_start,
                root_ended: entry.root_ended,
            })
            .collect();
        let written = (deletes.len(), upserts.len());
        let result = state
            .meta
            .write(move |conn| {
                Box::pin(async move { write_pending(conn, &deletes, &upserts).await })
            })
            .await;
        if let Err(error) = result {
            let mut rest = part;
            for later in parts {
                rest.upserts.extend(later.upserts);
                rest.deletes.extend(later.deletes);
            }
            state.pending.restore(rest);
            return Err(error);
        }
        report.deletes += written.0;
        report.upserts += written.1;
    }
    Ok(report)
}

/// Cuts `changes` into parts of at most `chunk` rows. The deletes come first, then the upserts.
fn flush_parts(changes: Changes, chunk: usize) -> Vec<Changes> {
    let mut parts = Vec::new();
    let mut current = Changes::default();
    let mut rows = 0;
    for delete in changes.deletes {
        if rows == chunk {
            parts.push(std::mem::take(&mut current));
            rows = 0;
        }
        current.deletes.push(delete);
        rows += 1;
    }
    for upsert in changes.upserts {
        if rows == chunk {
            parts.push(std::mem::take(&mut current));
            rows = 0;
        }
        current.upserts.push(upsert);
        rows += 1;
    }
    if rows > 0 {
        parts.push(current);
    }
    parts
}

/// A stored timeout in seconds as nanoseconds; out of range means the default.
fn timeout_ns(stored: Option<i64>) -> i64 {
    let seconds = stored
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| (COMPLETION_TIMEOUT_MIN_S..=COMPLETION_TIMEOUT_MAX_S).contains(value))
        .unwrap_or(COMPLETION_TIMEOUT_DEFAULT_S);
    i64::from(seconds) * NANOS_PER_SECOND
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_parts_puts_deletes_first_and_cuts_at_the_chunk() {
        let project = ProjectId::new_v7();
        let trace = |byte: u8| llmobs_core::ids::TraceId::from_bytes([byte; 16]).unwrap();
        let entry = Entry {
            first_seen: 1,
            last_seen: 1,
            min_start: 1,
            root_ended: false,
            failed_reads: 0,
            spans: 1,
        };
        let changes = Changes {
            upserts: (1..=4).map(|byte| (project, trace(byte), entry)).collect(),
            deletes: (10..=12).map(|byte| (project, trace(byte))).collect(),
        };
        let shape: Vec<(usize, usize)> = flush_parts(changes, 3)
            .iter()
            .map(|part| (part.deletes.len(), part.upserts.len()))
            .collect();
        assert_eq!(shape, vec![(3, 0), (0, 3), (0, 1)]);
    }

    #[test]
    fn timeouts_outside_the_range_mean_the_default() {
        assert_eq!(timeout_ns(None), 60 * NANOS_PER_SECOND);
        assert_eq!(timeout_ns(Some(5)), 60 * NANOS_PER_SECOND);
        assert_eq!(timeout_ns(Some(4_000)), 60 * NANOS_PER_SECOND);
        assert_eq!(timeout_ns(Some(-1)), 60 * NANOS_PER_SECOND);
        assert_eq!(timeout_ns(Some(120)), 120 * NANOS_PER_SECOND);
    }

    mod db {
        use std::collections::HashSet;

        use llmobs_core::ids::{OrgId, ProjectId, TraceId};
        use uuid::Uuid;

        use super::super::*;
        use crate::meta::evaluators::{EvaluatorFields, insert_evaluator};
        use crate::meta::online_rules::{NewRule, RuleFields, insert_rule};
        use crate::meta::pending_traces::{load_pending, write_pending};
        use crate::meta::projects::insert_project;
        use crate::test_support::{TestApp, dataset_span};

        const SECOND: i64 = 1_000_000_000;

        /// A span of the dataset without a parent, so its trace has a root.
        fn rooted_span() -> llmobs_core::span::Span {
            let mut span = dataset_span(0);
            span.parent_span_id = None;
            span.environment = None;
            span.metadata.clear();
            span
        }

        /// An enabled or disabled trace rule on `project`, with its evaluator.
        async fn add_rule(app: &TestApp, project: ProjectId, enabled: bool) {
            let evaluator = Uuid::now_v7();
            let rule = Uuid::now_v7();
            let now = app.state.clock.now_ns();
            app.state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        insert_evaluator(
                            conn,
                            evaluator,
                            project,
                            "check",
                            EvaluatorFields {
                                name: format!("Same {evaluator}"),
                                config: "{}".to_owned(),
                                output_score: "{}".to_owned(),
                                connection_id: None,
                                secret_enc: None,
                                version: 1,
                            },
                            None,
                            now,
                        )
                        .await?;
                        insert_rule(
                            conn,
                            NewRule {
                                id: rule,
                                project_id: project,
                                evaluator_id: evaluator,
                                target: "trace".to_owned(),
                                created_by: None,
                            },
                            RuleFields {
                                name: format!("Rule {rule}"),
                                filter: None,
                                variable_mapping: "{}".to_owned(),
                                sample_rate: 1.0,
                                budget_usd: None,
                                budget_period: None,
                                enabled,
                            },
                            now,
                        )
                        .await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
        }

        #[tokio::test]
        async fn flush_writes_changes_and_deletes() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            app.state
                .pending
                .set_tracked(HashSet::from([seeded.project.id]));
            app.commit_spans(&seeded.project, vec![rooted_span()]).await;
            let written = flush_once(&app.state).await.unwrap();
            assert_eq!(
                written,
                FlushReport {
                    upserts: 1,
                    deletes: 0
                }
            );
            let rows = load_pending(app.state.meta.reader()).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert!(rows[0].root_ended);
            // Ten seconds after the commit the rooted trace is complete; finish removes it.
            let now = app.state.clock.now_ns() + 10 * SECOND;
            let done = app
                .state
                .pending
                .completed(seeded.project.id, now, 60 * SECOND, 10);
            app.state.pending.finish(seeded.project.id, &done);
            let written = flush_once(&app.state).await.unwrap();
            assert_eq!(
                written,
                FlushReport {
                    upserts: 0,
                    deletes: 1
                }
            );
            assert!(
                load_pending(app.state.meta.reader())
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                flush_once(&app.state).await.unwrap(),
                FlushReport::default()
            );
        }

        #[tokio::test]
        async fn refresh_rules_tracks_the_projects_of_enabled_rules() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let project = seeded.project.id;
            add_rule(&app, project, false).await;
            assert!(refresh_rules(&app.state).await.unwrap().is_empty());
            add_rule(&app, project, true).await;
            let tracked = refresh_rules(&app.state).await.unwrap();
            assert_eq!(tracked.len(), 1);
            assert_eq!(tracked[&project].rules.len(), 1);
            assert_eq!(tracked[&project].timeout_ns, 60 * SECOND);

            for (settings, seconds) in [
                (r#"{"trace_completion_timeout_s": 120}"#, 120),
                (r#"{"trace_completion_timeout_s": 5}"#, 60),
            ] {
                app.state
                    .meta
                    .write(move |conn| {
                        Box::pin(async move {
                            sqlx::query("UPDATE projects SET settings = ? WHERE id = ?")
                                .bind(settings)
                                .bind(project.to_string())
                                .execute(&mut *conn)
                                .await?;
                            Ok(())
                        })
                    })
                    .await
                    .unwrap();
                let tracked = refresh_rules(&app.state).await.unwrap();
                assert_eq!(tracked[&project].timeout_ns, seconds * SECOND);
            }
        }

        #[tokio::test]
        async fn load_restores_tracked_entries_and_deletes_the_rest() {
            let app = TestApp::new().await;
            let seeded = app.seed().await;
            let now = app.state.clock.now_ns();
            let org: OrgId = seeded.org_id;
            let tracked_project = seeded.project.id;
            add_rule(&app, tracked_project, true).await;
            let other = app
                .state
                .meta
                .write(move |conn| {
                    Box::pin(async move {
                        Ok(insert_project(conn, org, "Other", "other", None, now)
                            .await?
                            .id)
                    })
                })
                .await
                .unwrap();
            let row = |project: ProjectId, byte: u8| PendingRow {
                project_id: project,
                trace_id: TraceId::from_bytes([byte; 16]).unwrap(),
                first_seen: now,
                last_seen: now,
                min_start: 0,
                root_ended: false,
            };
            let rows = vec![row(tracked_project, 1), row(other, 2)];
            app.state
                .meta
                .write(move |conn| Box::pin(async move { write_pending(conn, &[], &rows).await }))
                .await
                .unwrap();

            load(&app.state).await.unwrap();
            assert_eq!(app.state.pending.len(), 1);
            let written = flush_once(&app.state).await.unwrap();
            assert_eq!(
                written,
                FlushReport {
                    upserts: 0,
                    deletes: 1
                }
            );
            let left = load_pending(app.state.meta.reader()).await.unwrap();
            assert_eq!(left.len(), 1);
            assert_eq!(left[0].project_id, tracked_project);
        }
    }
}
