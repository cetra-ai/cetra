//! One evaluation pass over the due rules, and the rule state machine (spec 07, D-K-3, D-K-4).

use std::time::Duration;

use cetra_core::ids::{OrgId, ProjectId};
use cetra_storage::{
    AlertMeasure, AlertValue, AlertValueRequest, MAX_COUNT_NAMES, QueryContext, RunError,
    TimeWindow, alert_value,
};
use futures::StreamExt;
use uuid::Uuid;

use crate::alerts::condition::measure;
use crate::api::alerts::{AlertCondition, AlertEventKind, AlertMetric, AlertState};
use crate::entitlements::Visibility;
use crate::http::AppState;
use crate::http::error::ApiError;
use crate::meta::MetaError;
use crate::meta::alerts::{
    Evaluation, EventDetails, Recorded, RuleRow, due_rules, record_evaluation,
};
use crate::meta::issue_links::{issue_scope_ids, source_scope_ids};

/// Rules evaluated at once (D-K-3).
pub(crate) const CONCURRENCY: usize = 4;
/// Time allowed for one rule's query (D-K-3).
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// A rule is due this much early, so a 60 s tick never skips a 60 s interval (D-K-3).
pub(crate) const DUE_SLACK_NS: i64 = 5_000_000_000;
/// Clear or insufficient evaluations in a row that end a firing rule: two clear ones resolve
/// it, two insufficient ones move it to `no_data` (D-K-4, D-K-20).
pub(crate) const CLEARS_TO_RESOLVE: i64 = 2;
/// The `clear_streak` of a `no_data` rule that was firing when its data stopped (D-K-20). A
/// `no_data` rule that was never firing has 0.
pub(crate) const WENT_QUIET: i64 = 1;

/// The state after one successful evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) state: AlertState,
    pub(crate) clear_streak: i64,
    /// The event to record, if any.
    pub(crate) event: Option<AlertEventKind>,
    /// This evaluation fired the rule: `last_fired_at = now`.
    pub(crate) fired: bool,
    /// A `fired` or `reminder` event: `last_notified_at = now`.
    pub(crate) notified: bool,
}

/// How one measurement reads against the condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reading {
    /// Fewer samples than `min_count`.
    Insufficient,
    /// Enough samples, a value, and the comparator holds.
    Breaching,
    /// Anything else.
    Clear,
}

/// Classifies a measurement against `condition`. `issue_occurrences` has no `min_count` (its
/// normalization clears it), so its reading is `Breaching` or `Clear`, never `Insufficient`; zero
/// occurrences is a value and can breach (D-IC-20).
pub(crate) fn reading(condition: &AlertCondition, count: i64, value: Option<f64>) -> Reading {
    if condition.min_count.is_some_and(|min| count < min) {
        Reading::Insufficient
    } else if value.is_some_and(|value| condition.comparator.breaches(value, condition.threshold)) {
        Reading::Breaching
    } else {
        Reading::Clear
    }
}

/// The next state of `rule` after `reading` at `now` (D-K-4, the table in spec 07).
pub(crate) fn step(rule: &RuleRow, reading: Reading, now: i64) -> Step {
    match (rule.state, reading) {
        // A rule that went quiet while firing keeps `clear_streak` at `WENT_QUIET` (D-K-20).
        (AlertState::NoData, Reading::Clear) if rule.clear_streak > 0 => {
            quiet(AlertState::Ok, 0, Some(AlertEventKind::Resolved))
        }
        (AlertState::NoData, Reading::Insufficient) if rule.clear_streak > 0 => {
            quiet(AlertState::NoData, WENT_QUIET, None)
        }
        (AlertState::Ok | AlertState::NoData, Reading::Breaching) => Step {
            state: AlertState::Firing,
            clear_streak: 0,
            event: Some(AlertEventKind::Fired),
            fired: true,
            notified: true,
        },
        (AlertState::Ok | AlertState::NoData, Reading::Clear) => quiet(AlertState::Ok, 0, None),
        (AlertState::Ok | AlertState::NoData, Reading::Insufficient) => {
            quiet(AlertState::NoData, 0, None)
        }
        (AlertState::Firing, Reading::Breaching) => {
            if reminder_due(rule, now) {
                Step {
                    state: AlertState::Firing,
                    clear_streak: 0,
                    event: Some(AlertEventKind::Reminder),
                    fired: false,
                    notified: true,
                }
            } else {
                quiet(AlertState::Firing, 0, None)
            }
        }
        (AlertState::Firing, Reading::Clear | Reading::Insufficient) => {
            let streak = rule.clear_streak.saturating_add(1);
            if streak < CLEARS_TO_RESOLVE {
                quiet(AlertState::Firing, streak, None)
            } else if reading == Reading::Insufficient {
                // No data is not a recovery (D-K-20): the rule is not resolved.
                quiet(AlertState::NoData, WENT_QUIET, Some(AlertEventKind::NoData))
            } else {
                quiet(AlertState::Ok, 0, Some(AlertEventKind::Resolved))
            }
        }
    }
}

/// A step that neither fires nor notifies.
fn quiet(state: AlertState, clear_streak: i64, event: Option<AlertEventKind>) -> Step {
    Step {
        state,
        clear_streak,
        event,
        fired: false,
        notified: false,
    }
}

/// A firing rule with reminders on sends one once its cooldown has passed since the last
/// notification.
fn reminder_due(rule: &RuleRow, now: i64) -> bool {
    rule.notify_while_firing
        && rule.last_notified_at.is_none_or(|at| {
            now.saturating_sub(at) >= rule.cooldown_s.saturating_mul(1_000_000_000)
        })
}

/// What one pass did. Logged by the loop (K9) and asserted by tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EvaluateReport {
    pub(crate) evaluated: usize,
    pub(crate) failed: usize,
    pub(crate) events: usize,
    pub(crate) stale: usize,
}

/// Evaluates every due rule once at `now`, at most [`CONCURRENCY`] at a time.
///
/// # Errors
///
/// Returns [`MetaError`] only when the due rules cannot be read. A failed query or a failed
/// write is logged and counted in the report.
pub(crate) async fn evaluate_once(state: &AppState, now: i64) -> Result<EvaluateReport, MetaError> {
    let due = due_rules(state.meta.reader(), now, DUE_SLACK_NS).await?;
    let results = futures::stream::iter(due)
        .map(|(org, rule)| Box::pin(evaluate_rule(state, org, rule, now)))
        .buffer_unordered(CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    let mut report = EvaluateReport::default();
    for (rule_id, query_failed, written) in results {
        report.evaluated += 1;
        let mut failed = query_failed;
        match written {
            Ok(Recorded::Event { .. }) => report.events += 1,
            Ok(Recorded::Stale) => report.stale += 1,
            Ok(Recorded::Written) => {}
            Err(error) => {
                tracing::error!(rule_id = %rule_id, error = %error, "alert evaluation write failed");
                failed = true;
            }
        }
        if failed {
            report.failed += 1;
        }
    }
    state.telemetry.alert_evaluated(
        report.evaluated.saturating_sub(report.failed),
        report.failed,
    );
    Ok(report)
}

/// Evaluates one rule. Returns its id, whether its query failed, and the write's result.
async fn evaluate_rule(
    state: &AppState,
    org: OrgId,
    rule: RuleRow,
    now: i64,
) -> (Uuid, bool, Result<Recorded, MetaError>) {
    let rule_id = rule.id;
    let project = rule.project_id;
    let expected_updated_at = rule.updated_at;
    let window_from = now.saturating_sub(rule.condition.window.ns());
    let mut event_kind = None;
    let evaluation =
        match measure_rule(state, org, project, &rule.condition, window_from, now).await {
            Err(reason) => {
                tracing::warn!(rule_id = %rule_id, reason, "alert evaluation failed");
                Evaluation::Failed {
                    message: reason.to_owned(),
                }
            }
            Ok(measured) => {
                let read = reading(&rule.condition, measured.count, measured.value);
                let next = step(&rule, read, now);
                event_kind = next.event;
                Evaluation::Measured {
                    step: next,
                    count: measured.count,
                    value: measured.value,
                    threshold: rule.condition.threshold,
                    window_from,
                    window_to: now,
                    details: EventDetails {
                        rule_name: rule.name,
                        metric: rule.condition.metric,
                        comparator: rule.condition.comparator,
                        window: rule.condition.window,
                        filter: rule.condition.filter,
                    },
                }
            }
        };
    let query_failed = matches!(evaluation, Evaluation::Failed { .. });
    let written = state
        .meta
        .write(move |conn| {
            Box::pin(async move {
                record_evaluation(conn, rule_id, expected_updated_at, evaluation, now).await
            })
        })
        .await;
    if let Ok(Recorded::Event {
        event_id,
        deliveries,
    }) = &written
    {
        tracing::info!(
            rule_id = %rule_id,
            event_id = %event_id,
            kind = event_kind.map_or("", AlertEventKind::as_str),
            deliveries,
            "alert event"
        );
    }
    (rule_id, query_failed, written)
}

/// Measures the rule's condition over `[from, to)`. The error is a fixed reason, never a
/// filter or a query message.
async fn measure_rule(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    condition: &AlertCondition,
    from: i64,
    to: i64,
) -> Result<AlertValue, &'static str> {
    let window = TimeWindow::new(from, to).map_err(|_| "invalid window")?;
    if condition.metric == AlertMetric::IssueOccurrences {
        return measure_issue_rule(state, org, project, condition, window).await;
    }
    let ctx = open_ctx(state, org, project).await?;
    // Every other metric has a storage measure, so a stored rule never takes this `None` branch.
    let Some(measure) = measure(condition) else {
        return Err("query failed");
    };
    let request = AlertValueRequest {
        window,
        filter: condition.filter.as_deref(),
        measure,
        timeout: QUERY_TIMEOUT,
    };
    Box::pin(alert_value(&ctx, request))
        .await
        .map_err(|error| run_reason(&error))
}

/// Opens the project's storage for a measurement, as the rule's org sees it (D-P-21). The error
/// is a fixed reason.
async fn open_ctx(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
) -> Result<QueryContext, &'static str> {
    crate::routes::read::open_query_on(
        &state.storage,
        project,
        Visibility::window(state, org),
        None,
        None,
    )
    .await
    .map_err(|error| match error {
        ApiError::Unavailable { .. } => "storage is busy",
        _ => "query failed",
    })
}

/// The fixed reason of a failed measurement.
fn run_reason(error: &RunError) -> &'static str {
    match error {
        RunError::Timeout => "query timed out",
        RunError::TooLarge => "query needed more memory or temporary disk space than allowed",
        RunError::Busy => "storage is busy",
        RunError::Invalid(_) => "the filter is no longer valid",
        _ => "query failed",
    }
}

/// Counts the occurrence scores of the rule's issue or source over `window` (D-IC-20): the
/// names are resolved from `meta.db` now, split in chunks of `MAX_COUNT_NAMES`, one
/// `alert_value` each, and the counts are summed.
async fn measure_issue_rule(
    state: &AppState,
    org: OrgId,
    project: ProjectId,
    condition: &AlertCondition,
    window: TimeWindow,
) -> Result<AlertValue, &'static str> {
    let ids = match (condition.issue_id, condition.source_id) {
        (Some(issue), None) => issue_scope_ids(state.meta.reader(), project, issue)
            .await
            .map_err(|_| "query failed")?
            .ok_or("issue not found")?,
        (None, Some(source)) => source_scope_ids(state.meta.reader(), project, source)
            .await
            .map_err(|_| "query failed")?
            .ok_or("issue source not found")?,
        // The API refuses this shape, so a stored row is never both or neither.
        _ => return Err("query failed"),
    };
    let names: Vec<String> = ids.iter().map(|id| format!("issue:{id}")).collect();
    if names.is_empty() {
        return Ok(AlertValue {
            count: 0,
            value: Some(0.0),
        });
    }
    let ctx = open_ctx(state, org, project).await?;
    let mut total: i64 = 0;
    for chunk in names.chunks(MAX_COUNT_NAMES) {
        let request = AlertValueRequest {
            window,
            filter: None,
            measure: AlertMeasure::ScoreCount {
                names: chunk.to_vec(),
            },
            timeout: QUERY_TIMEOUT,
        };
        let value = Box::pin(alert_value(&ctx, request))
            .await
            .map_err(|error| run_reason(&error))?;
        total = total.saturating_add(value.count);
    }
    Ok(AlertValue {
        count: total,
        value: Some(total as f64),
    })
}

#[cfg(test)]
mod tests {
    use cetra_core::enums::SpanStatus;
    use cetra_core::ids::{SpanId, TraceId};
    use cetra_core::span::Span;

    use axum::http::Method;
    use cetra_eval::{Secret, sign_payload};
    use serde_json::{Value, json};

    use super::*;
    use crate::alerts::deliver::DELIVERY_HEADER;
    use crate::alerts::{AlertTiming, spawn_loops};
    use crate::api::alerts::{AlertMetric, AlertWindow, ChannelKind, Comparator, DeliveryStatus};
    use crate::meta::alerts::{
        NewChannel, NewRule, RulePatch, deliveries_for_events, insert_channel, insert_rule,
        list_events, rule_by_id, update_rule,
    };
    use crate::meta::issues::{IssueRow, IssueState, SourceKind, insert_issue};
    use crate::test_support::hook_receiver::HookReceiver;
    use crate::test_support::issues::{issue_row, occurrence, put_issue, put_source, source_row};
    use crate::test_support::{TestApp, assert_logs_clean, dataset_span, set_limits};

    const MINUTE: Duration = Duration::from_secs(60);
    const MINUTE_NS: i64 = 60_000_000_000;

    fn rule(state: AlertState, streak: i64) -> RuleRow {
        RuleRow {
            id: Uuid::now_v7(),
            project_id: ProjectId::new_v7(),
            name: "r".to_owned(),
            condition: error_rate(AlertWindow::M5, 0.1, Some(10), None),
            channel_ids: Vec::new(),
            enabled: true,
            cooldown_s: 3600,
            notify_while_firing: false,
            state,
            clear_streak: streak,
            last_value: None,
            last_count: None,
            last_evaluated_at: None,
            last_fired_at: None,
            last_notified_at: None,
            last_error: None,
            created_by: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn error_rate(
        window: AlertWindow,
        threshold: f64,
        min_count: Option<i64>,
        filter: Option<&str>,
    ) -> AlertCondition {
        AlertCondition {
            metric: AlertMetric::ErrorRate,
            window,
            filter: filter.map(str::to_owned),
            comparator: Comparator::Gt,
            threshold,
            min_count,
            target: None,
            score_name: None,
            aggregation: None,
            issue_id: None,
            source_id: None,
        }
    }

    #[test]
    fn reading_classifies() {
        let condition = error_rate(AlertWindow::M5, 0.1, Some(10), None);
        assert_eq!(reading(&condition, 9, Some(0.2)), Reading::Insufficient);
        assert_eq!(reading(&condition, 10, Some(0.2)), Reading::Breaching);
        assert_eq!(reading(&condition, 10, Some(0.1)), Reading::Clear);
        assert_eq!(reading(&condition, 10, None), Reading::Clear);

        let cost = AlertCondition {
            metric: AlertMetric::Cost,
            min_count: None,
            comparator: Comparator::Gte,
            threshold: 0.0,
            ..error_rate(AlertWindow::M5, 0.0, None, None)
        };
        assert_eq!(reading(&cost, 0, Some(0.0)), Reading::Breaching);
    }

    #[test]
    fn step_table() {
        let now = 1_000 * MINUTE_NS;
        let fired = Step {
            state: AlertState::Firing,
            clear_streak: 0,
            event: Some(AlertEventKind::Fired),
            fired: true,
            notified: true,
        };
        for state in [AlertState::Ok, AlertState::NoData] {
            assert_eq!(
                step(&rule(state, 0), Reading::Breaching, now),
                fired,
                "{state:?}"
            );
            assert_eq!(
                step(&rule(state, 0), Reading::Clear, now),
                quiet(AlertState::Ok, 0, None)
            );
            assert_eq!(
                step(&rule(state, 0), Reading::Insufficient, now),
                quiet(AlertState::NoData, 0, None)
            );
        }

        let mut reminding = rule(AlertState::Firing, 0);
        reminding.notify_while_firing = true;
        reminding.last_notified_at = None;
        assert_eq!(
            step(&reminding, Reading::Breaching, now),
            Step {
                state: AlertState::Firing,
                clear_streak: 0,
                event: Some(AlertEventKind::Reminder),
                fired: false,
                notified: true,
            }
        );
        assert_eq!(
            step(&rule(AlertState::Firing, 0), Reading::Breaching, now),
            quiet(AlertState::Firing, 0, None)
        );
        assert_eq!(
            step(&rule(AlertState::Firing, 0), Reading::Clear, now),
            quiet(AlertState::Firing, 1, None)
        );
        assert_eq!(
            step(&rule(AlertState::Firing, 0), Reading::Insufficient, now),
            quiet(AlertState::Firing, 1, None)
        );
        assert_eq!(
            step(&rule(AlertState::Firing, 1), Reading::Clear, now),
            quiet(AlertState::Ok, 0, Some(AlertEventKind::Resolved))
        );
        // Two insufficient readings are no data, never a resolution (D-K-20).
        assert_eq!(
            step(&rule(AlertState::Firing, 1), Reading::Insufficient, now),
            quiet(AlertState::NoData, WENT_QUIET, Some(AlertEventKind::NoData))
        );
        // A rule that went quiet while firing: data back decides.
        let quiet_rule = rule(AlertState::NoData, WENT_QUIET);
        assert_eq!(step(&quiet_rule, Reading::Breaching, now), fired);
        assert_eq!(
            step(&quiet_rule, Reading::Clear, now),
            quiet(AlertState::Ok, 0, Some(AlertEventKind::Resolved))
        );
        assert_eq!(
            step(&quiet_rule, Reading::Insufficient, now),
            quiet(AlertState::NoData, WENT_QUIET, None)
        );
    }

    #[test]
    fn reminders_respect_the_cooldown() {
        let now = 10_000 * 1_000_000_000;
        let mut firing = rule(AlertState::Firing, 0);
        firing.notify_while_firing = true;
        firing.last_notified_at = Some(now - 3599 * 1_000_000_000);
        assert_eq!(
            step(&firing, Reading::Breaching, now).event,
            None,
            "inside the cooldown"
        );

        firing.last_notified_at = Some(now - 3600 * 1_000_000_000);
        let reminder = step(&firing, Reading::Breaching, now);
        assert_eq!(reminder.event, Some(AlertEventKind::Reminder));
        assert!(reminder.notified);

        firing.notify_while_firing = false;
        assert_eq!(step(&firing, Reading::Breaching, now).event, None);
    }

    fn root_span(app: &TestApp, index: u8, ago: Duration, error: bool) -> Span {
        let mut span = dataset_span(0);
        let start = app.state.clock.now_ns() - i64::try_from(ago.as_nanos()).expect("small");
        span.trace_id = TraceId::from_bytes([index; 16]).expect("trace id");
        span.span_id = SpanId::from_bytes([index; 8]).expect("span id");
        span.parent_span_id = None;
        span.start_time = start;
        span.end_time = start + 1_000_000;
        span.status = if error {
            SpanStatus::Error
        } else {
            SpanStatus::Ok
        };
        span.environment = None;
        span.metadata.clear();
        span
    }

    fn assert_close(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("a value");
        assert!(
            (actual - expected).abs() < 1e-9,
            "{actual} is not {expected}"
        );
    }

    async fn add_channel(app: &TestApp, project: ProjectId, name: &str) -> Uuid {
        let channel = NewChannel {
            id: Uuid::now_v7(),
            project_id: project,
            kind: ChannelKind::Webhook,
            name: name.to_owned(),
            config_enc: vec![1, 2, 3],
            target_hint: "hooks.example.com".to_owned(),
        };
        let id = channel.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move { insert_channel(conn, channel, 1).await.map(|_| ()) })
            })
            .await
            .expect("channel");
        id
    }

    async fn add_rule(
        app: &TestApp,
        project: ProjectId,
        name: &str,
        condition: AlertCondition,
        channel_ids: Vec<Uuid>,
    ) -> RuleRow {
        let rule = NewRule {
            project_id: project,
            name: name.to_owned(),
            condition,
            channel_ids,
            enabled: true,
            cooldown_s: 3600,
            notify_while_firing: false,
            created_by: None,
        };
        let now = app.state.clock.now_ns();
        app.state
            .meta
            .write(move |conn| Box::pin(async move { insert_rule(conn, rule, now).await }))
            .await
            .expect("rule")
    }

    async fn read_rule(app: &TestApp, project: ProjectId, id: Uuid) -> RuleRow {
        rule_by_id(app.state.meta.reader(), project, id)
            .await
            .expect("query")
            .expect("rule")
    }

    #[tokio::test]
    async fn fires_then_resolves_with_the_manual_clock() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let channel = add_channel(&app, project, "ops").await;
        let rule = add_rule(
            &app,
            project,
            "errors",
            error_rate(AlertWindow::M5, 0.4, Some(2), None),
            vec![channel],
        )
        .await;
        app.commit_spans(
            &seeded.project,
            vec![
                root_span(&app, 1, MINUTE, true),
                root_span(&app, 2, MINUTE, true),
                root_span(&app, 3, MINUTE, false),
            ],
        )
        .await;

        let now = app.state.clock.now_ns();
        let report = evaluate_once(&app.state, now).await.unwrap();
        assert_eq!(
            report,
            EvaluateReport {
                evaluated: 1,
                events: 1,
                ..EvaluateReport::default()
            }
        );
        let row = read_rule(&app, project, rule.id).await;
        assert_eq!(row.state, AlertState::Firing);
        assert_close(row.last_value, 2.0 / 3.0);
        assert_eq!(row.last_count, Some(3));
        let events = list_events(app.state.meta.reader(), rule.id, None, 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        let fired = &events[0];
        assert_eq!(fired.kind, AlertEventKind::Fired);
        assert_close(fired.value, 2.0 / 3.0);
        assert_eq!(fired.count, Some(3));
        assert_eq!(fired.threshold, 0.4);
        assert_eq!(fired.window_to, now);
        assert_eq!(fired.window_from, now - 5 * MINUTE_NS);
        let deliveries = deliveries_for_events(app.state.meta.reader(), &[fired.id])
            .await
            .unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].status, DeliveryStatus::Pending);

        // Not due yet: 30 s is under the 55 s due point.
        app.state.clock.advance(Duration::from_secs(30));
        let now = app.state.clock.now_ns();
        assert_eq!(
            evaluate_once(&app.state, now).await.unwrap(),
            EvaluateReport::default()
        );

        // Due again and still breaching: no new event (no reminders).
        app.state.clock.advance(Duration::from_secs(30));
        let now = app.state.clock.now_ns();
        assert_eq!(
            evaluate_once(&app.state, now).await.unwrap(),
            EvaluateReport {
                evaluated: 1,
                ..EvaluateReport::default()
            }
        );
        let events = list_events(app.state.meta.reader(), rule.id, None, 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 1);

        // The traces leave the window: insufficient, first clear evaluation.
        app.state.clock.advance(Duration::from_secs(6 * 60));
        let now = app.state.clock.now_ns();
        assert_eq!(
            evaluate_once(&app.state, now).await.unwrap(),
            EvaluateReport {
                evaluated: 1,
                ..EvaluateReport::default()
            }
        );
        let row = read_rule(&app, project, rule.id).await;
        assert_eq!(row.state, AlertState::Firing);
        assert_eq!(row.clear_streak, 1);

        // The second one moves the rule to no_data, not to resolved (D-K-20).
        app.state.clock.advance(Duration::from_secs(60));
        let now = app.state.clock.now_ns();
        assert_eq!(
            evaluate_once(&app.state, now).await.unwrap(),
            EvaluateReport {
                evaluated: 1,
                events: 1,
                ..EvaluateReport::default()
            }
        );
        let row = read_rule(&app, project, rule.id).await;
        assert_eq!(row.state, AlertState::NoData);
        assert_eq!(row.clear_streak, WENT_QUIET);
        let events = list_events(app.state.meta.reader(), rule.id, None, 10)
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
        let resolved = &events[0];
        assert_eq!(resolved.kind, AlertEventKind::NoData);
        assert_eq!(resolved.value, None);
        assert_eq!(resolved.count, Some(0));
        let ids: Vec<Uuid> = events.iter().map(|event| event.id).collect();
        let deliveries = deliveries_for_events(app.state.meta.reader(), &ids)
            .await
            .unwrap();
        assert_eq!(deliveries.len(), 2);
        assert!(
            deliveries
                .iter()
                .all(|delivery| delivery.status == DeliveryStatus::Pending)
        );
    }

    #[tokio::test]
    async fn a_failed_query_records_the_error() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let rule = add_rule(
            &app,
            project,
            "broken",
            error_rate(AlertWindow::M5, 0.4, Some(2), Some("nonsense_column = 1")),
            Vec::new(),
        )
        .await;
        let now = app.state.clock.now_ns();

        let report = evaluate_once(&app.state, now).await.unwrap();
        assert_eq!(
            report,
            EvaluateReport {
                evaluated: 1,
                failed: 1,
                ..EvaluateReport::default()
            }
        );
        let row = read_rule(&app, project, rule.id).await;
        assert_eq!(row.state, AlertState::Ok);
        assert_eq!(
            row.last_error.as_deref(),
            Some("the filter is no longer valid")
        );
        assert_eq!(row.last_evaluated_at, Some(now));
        let events = list_events(app.state.meta.reader(), rule.id, None, 10)
            .await
            .unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn an_edit_during_evaluation_is_not_overwritten() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let rule = add_rule(
            &app,
            project,
            "edited",
            error_rate(AlertWindow::M5, 0.4, Some(2), None),
            Vec::new(),
        )
        .await;
        let now = app.state.clock.now_ns();
        let read = read_rule(&app, project, rule.id).await;
        let id = rule.id;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    update_rule(
                        conn,
                        project,
                        id,
                        RulePatch {
                            name: Some("renamed".to_owned()),
                            ..RulePatch::default()
                        },
                        now + 1,
                    )
                    .await
                    .map(|_| ())
                })
            })
            .await
            .unwrap();

        let recorded = app
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    record_evaluation(
                        conn,
                        id,
                        read.updated_at,
                        Evaluation::Failed {
                            message: "x".to_owned(),
                        },
                        now,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        assert_eq!(recorded, Recorded::Stale);
        let row = read_rule(&app, project, id).await;
        assert_eq!(row.name, "renamed");
        assert_eq!(row.last_error, None);
    }

    #[tokio::test]
    async fn due_rules_order_and_stale_count() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let broken = add_rule(
            &app,
            project,
            "broken",
            error_rate(AlertWindow::M5, 0.4, Some(2), Some("nonsense_column = 1")),
            Vec::new(),
        )
        .await;
        let quiet_rule = add_rule(
            &app,
            project,
            "quiet",
            error_rate(AlertWindow::M5, 0.4, Some(2), None),
            Vec::new(),
        )
        .await;
        let old = add_rule(
            &app,
            project,
            "old",
            error_rate(AlertWindow::M5, 0.4, Some(2), None),
            Vec::new(),
        )
        .await;
        set_last_evaluated(&app, old.id, now - 3600 * 1_000_000_000).await;

        // Never evaluated first, by id; then the rule evaluated long ago.
        let due: Vec<Uuid> = due_rules(app.state.meta.reader(), now, DUE_SLACK_NS)
            .await
            .unwrap()
            .iter()
            .map(|(_, rule)| rule.id)
            .collect();
        let mut never = [broken.id, quiet_rule.id];
        never.sort();
        assert_eq!(due, vec![never[0], never[1], old.id]);

        // The old rule was evaluated 30 s ago, so it is not due; the other two are.
        set_last_evaluated(&app, old.id, now - 30 * 1_000_000_000).await;
        let report = evaluate_once(&app.state, now).await.unwrap();
        assert_eq!(
            report,
            EvaluateReport {
                evaluated: 2,
                failed: 1,
                events: 0,
                stale: 0,
            }
        );
    }

    /// Sets `id`'s `last_evaluated_at` by SQL.
    async fn set_last_evaluated(app: &TestApp, id: Uuid, at: i64) {
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE alert_rules SET last_evaluated_at = ? WHERE id = ?")
                        .bind(at)
                        .bind(id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn evaluation_logs_hold_no_filter() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let rule = add_rule(
            &app,
            project,
            "leaky",
            error_rate(
                AlertWindow::M5,
                0.4,
                Some(2),
                Some("missing_column = 'secret-env'"),
            ),
            Vec::new(),
        )
        .await;
        let now = app.state.clock.now_ns();

        let report = assert_logs_clean(
            &["secret-env", "missing_column"],
            evaluate_once(&app.state, now),
        )
        .await
        .unwrap();
        assert_eq!(report.failed, 1);
        let row = read_rule(&app, project, rule.id).await;
        assert_eq!(
            row.last_error.as_deref(),
            Some("the filter is no longer valid")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rule_to_signed_webhook_end_to_end() {
        const SECRET: &str = "0123456789abcdef";
        let app =
            TestApp::with_config(|config| config.security.allow_private_network_egress = true)
                .await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let receiver = HookReceiver::start(vec![200]).await;

        let channels = format!("/api/v1/projects/{project}/channels");
        let channel = app
            .request(Method::POST, &channels)
            .header("cookie", &seeded.cookie)
            .json(&json!({
                "name": "Hook",
                "kind": "webhook",
                "url": receiver.url("/hook"),
                "secret": SECRET,
            }))
            .send()
            .await;
        assert_eq!(channel.status, 201, "{:?}", channel.body);
        let channel_id = channel.json::<Value>()["id"]
            .as_str()
            .expect("channel id")
            .to_owned();

        let rules = format!("/api/v1/projects/{project}/alert-rules");
        let created = app
            .request(Method::POST, &rules)
            .header("cookie", &seeded.cookie)
            .json(&json!({
                "name": "Errors",
                "condition": {
                    "metric": "error_rate",
                    "window": "1m",
                    "comparator": "gt",
                    "threshold": 0.4,
                    "min_count": 1,
                },
                "channel_ids": [channel_id],
            }))
            .send()
            .await;
        assert_eq!(created.status, 201, "{:?}", created.body);
        let rule_id = created.json::<Value>()["id"]
            .as_str()
            .expect("rule id")
            .to_owned();

        app.commit_spans(
            &seeded.project,
            vec![
                root_span(&app, 1, Duration::from_secs(10), false),
                root_span(&app, 2, Duration::from_secs(10), true),
            ],
        )
        .await;

        // The manual clock does not move: the rule is due once (never evaluated), and its
        // delivery is due at the same `now`.
        let tasks = spawn_loops(&app.state, AlertTiming::TEST);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while receiver.received().is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "no webhook was sent"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Later passes are not due, so nothing else is sent.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let requests = receiver.received();
        assert_eq!(requests.len(), 1);

        // The delivery pass may write its `sent` status after the receiver recorded the
        // request, so poll the history for up to 2 s.
        let events_path = format!("{rules}/{rule_id}/events");
        let mut page = Value::Null;
        for _ in 0..40 {
            page = app
                .request(Method::GET, &events_path)
                .header("cookie", &seeded.cookie)
                .send()
                .await
                .json::<Value>();
            if page["data"][0]["deliveries"][0]["status"] == "sent" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let events = page["data"].as_array().expect("events");
        assert_eq!(events.len(), 1, "{page}");
        assert_eq!(events[0]["kind"], "fired");
        let deliveries = events[0]["deliveries"].as_array().expect("deliveries");
        assert_eq!(deliveries.len(), 1, "{page}");
        assert_eq!(deliveries[0]["status"], "sent");
        assert_eq!(deliveries[0]["attempts"], 1);
        let delivery_id: Uuid = deliveries[0]["id"]
            .as_str()
            .expect("delivery id")
            .parse()
            .expect("uuid");

        let rule = app
            .request(Method::GET, &format!("{rules}/{rule_id}"))
            .header("cookie", &seeded.cookie)
            .send()
            .await
            .json::<Value>();
        assert_eq!(rule["state"], "firing");

        let request = &requests[0];
        assert_eq!(request.path, "/hook");
        let body: Value = serde_json::from_slice(&request.body).expect("json body");
        assert_eq!(body["type"], "alert.fired");
        assert_eq!(body["rule"]["id"], rule_id.as_str());
        assert_eq!(body["event_id"], events[0]["id"]);
        assert_eq!(body["count"], 2);
        assert_eq!(body["value"], 0.5);

        let signature = request
            .headers
            .get("x-cetra-signature")
            .and_then(|value| value.to_str().ok())
            .expect("signature header")
            .to_owned();
        let timestamp: i64 = signature
            .split(',')
            .find_map(|part| part.strip_prefix("t="))
            .expect("timestamp")
            .parse()
            .expect("timestamp is a number");
        let expected = sign_payload(&Secret::new(SECRET.to_owned()), timestamp, &request.body);
        assert_eq!(Some(signature.as_str()), expected.as_deref());

        let delivered = request
            .headers
            .get(DELIVERY_HEADER)
            .and_then(|value| value.to_str().ok())
            .expect("delivery header");
        assert_eq!(delivered, delivery_id.to_string());

        for task in tasks {
            task.abort();
            let _ = task.await;
        }
    }

    /// D-P-21: a rule's measurement counts only the rows the org's window shows. The rule window
    /// (two days) reaches past the one-day window.
    #[tokio::test(flavor = "multi_thread")]
    async fn alerts_do_not_count_hidden_spans() {
        const DAY: i64 = 86_400_000_000_000;
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        set_limits(
            &app,
            seeded.org_id,
            "2026-01-01T00:00:00Z",
            &json!({"data_access_days": 1}),
        );
        let now = app.state.clock.now_ns();
        let hidden = TraceId::from_bytes([0x91; 16]).expect("trace id");
        let visible = TraceId::from_bytes([0x92; 16]).expect("trace id");
        app.commit_trace_at(&seeded.project, hidden, 1, now - 30 * 3_600_000_000_000)
            .await;
        app.commit_trace_at(&seeded.project, visible, 2, now - 2 * 3_600_000_000_000)
            .await;

        let condition = error_rate(AlertWindow::M5, 0.1, None, None);
        let measured = measure_rule(
            &app.state,
            seeded.org_id,
            seeded.project.id,
            &condition,
            now - 2 * DAY,
            now,
        )
        .await
        .expect("the rule measures");
        assert_eq!(measured.count, 1);
    }

    /// An `issue_occurrences` condition; `min_count` is `None`, as normalization leaves it.
    fn issue_condition(
        window: AlertWindow,
        comparator: Comparator,
        threshold: f64,
        issue_id: Option<Uuid>,
        source_id: Option<Uuid>,
    ) -> AlertCondition {
        AlertCondition {
            metric: AlertMetric::IssueOccurrences,
            window,
            filter: None,
            comparator,
            threshold,
            min_count: None,
            target: None,
            score_name: None,
            aggregation: None,
            issue_id,
            source_id,
        }
    }

    /// A distinct 32-hex trace id per `index`.
    fn trace_hex(index: usize) -> String {
        format!("{index:032x}")
    }

    /// Stores an issue of `state` in `source`, merged into `into` when given.
    async fn put_merged_or_plain(
        app: &TestApp,
        project: ProjectId,
        source: Uuid,
        id: Uuid,
        state: IssueState,
        into: Option<Uuid>,
        now: i64,
    ) {
        let mut row: IssueRow = issue_row(project, source, id, "issue", state, now);
        row.merged_into = into;
        put_issue(app, row).await;
    }

    #[tokio::test]
    async fn issue_rule_counts_occurrences_in_the_window() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "errors", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        let issue = Uuid::now_v7();
        put_issue(
            &app,
            issue_row(project, source_id, issue, "boom", IssueState::Open, now),
        )
        .await;
        // Three occurrences inside the 15 minute window, two before it.
        for (index, minutes) in [2_i64, 3, 4, 20, 30].into_iter().enumerate() {
            occurrence(
                &app,
                &seeded,
                source_id,
                issue,
                &trace_hex(index + 1),
                now - minutes * MINUTE_NS,
                None,
                None,
            )
            .await;
        }

        let condition = issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, Some(issue), None);
        let measured = measure_rule(
            &app.state,
            seeded.org_id,
            project,
            &condition,
            now - 15 * MINUTE_NS,
            now,
        )
        .await
        .expect("the rule measures");
        assert_eq!(measured.count, 3);
        assert_eq!(measured.value, Some(3.0));
    }

    #[tokio::test]
    async fn issue_rule_counts_merged_issues_with_their_target() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "errors", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        let target = Uuid::now_v7();
        let merged = Uuid::now_v7();
        put_merged_or_plain(
            &app,
            project,
            source_id,
            target,
            IssueState::Open,
            None,
            now,
        )
        .await;
        put_merged_or_plain(
            &app,
            project,
            source_id,
            merged,
            IssueState::Merged,
            Some(target),
            now,
        )
        .await;
        occurrence(
            &app,
            &seeded,
            source_id,
            target,
            &trace_hex(1),
            now - MINUTE_NS,
            None,
            None,
        )
        .await;
        occurrence(
            &app,
            &seeded,
            source_id,
            merged,
            &trace_hex(2),
            now - 2 * MINUTE_NS,
            None,
            None,
        )
        .await;

        let condition = issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, Some(target), None);
        let measured = measure_rule(
            &app.state,
            seeded.org_id,
            project,
            &condition,
            now - 15 * MINUTE_NS,
            now,
        )
        .await
        .expect("the rule measures");
        assert_eq!(measured.count, 2);
    }

    #[tokio::test]
    async fn issue_rule_on_a_merged_issue_follows_its_target() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "errors", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        let target = Uuid::now_v7();
        let issue = Uuid::now_v7();
        put_merged_or_plain(
            &app,
            project,
            source_id,
            target,
            IssueState::Open,
            None,
            now,
        )
        .await;
        put_merged_or_plain(&app, project, source_id, issue, IssueState::Open, None, now).await;
        // The rule is made on `issue`, which is merged into `target` afterwards.
        let condition = issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, Some(issue), None);
        occurrence(
            &app,
            &seeded,
            source_id,
            issue,
            &trace_hex(1),
            now - MINUTE_NS,
            None,
            None,
        )
        .await;
        occurrence(
            &app,
            &seeded,
            source_id,
            target,
            &trace_hex(2),
            now - 2 * MINUTE_NS,
            None,
            None,
        )
        .await;
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("UPDATE issues SET state = 'merged', merged_into = ? WHERE id = ?")
                        .bind(target.to_string())
                        .bind(issue.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let measured = measure_rule(
            &app.state,
            seeded.org_id,
            project,
            &condition,
            now - 15 * MINUTE_NS,
            now,
        )
        .await
        .expect("the rule measures");
        assert_eq!(
            measured.count, 2,
            "the target's whole group counts (D-IC-20)"
        );
    }

    #[tokio::test]
    async fn issue_rule_counts_an_ignored_issue() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "errors", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        let ignored = Uuid::now_v7();
        put_merged_or_plain(
            &app,
            project,
            source_id,
            ignored,
            IssueState::Ignored,
            None,
            now,
        )
        .await;
        occurrence(
            &app,
            &seeded,
            source_id,
            ignored,
            &trace_hex(1),
            now - MINUTE_NS,
            None,
            None,
        )
        .await;

        let condition = issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, Some(ignored), None);
        let measured = measure_rule(
            &app.state,
            seeded.org_id,
            project,
            &condition,
            now - 15 * MINUTE_NS,
            now,
        )
        .await
        .expect("the rule measures");
        assert_eq!(measured.count, 1);
    }

    #[tokio::test]
    async fn source_rule_skips_ignored_and_merged_issues() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "errors", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        let open = Uuid::now_v7();
        let resolved = Uuid::now_v7();
        let target = Uuid::now_v7();
        let merged_into_open = Uuid::now_v7();
        let ignored = Uuid::now_v7();
        let merged_into_ignored = Uuid::now_v7();
        put_merged_or_plain(&app, project, source_id, open, IssueState::Open, None, now).await;
        put_merged_or_plain(
            &app,
            project,
            source_id,
            resolved,
            IssueState::Resolved,
            None,
            now,
        )
        .await;
        put_merged_or_plain(
            &app,
            project,
            source_id,
            target,
            IssueState::Open,
            None,
            now,
        )
        .await;
        put_merged_or_plain(
            &app,
            project,
            source_id,
            merged_into_open,
            IssueState::Merged,
            Some(target),
            now,
        )
        .await;
        put_merged_or_plain(
            &app,
            project,
            source_id,
            ignored,
            IssueState::Ignored,
            None,
            now,
        )
        .await;
        put_merged_or_plain(
            &app,
            project,
            source_id,
            merged_into_ignored,
            IssueState::Merged,
            Some(ignored),
            now,
        )
        .await;
        for (index, issue) in [
            open,
            resolved,
            target,
            merged_into_open,
            ignored,
            merged_into_ignored,
        ]
        .into_iter()
        .enumerate()
        {
            occurrence(
                &app,
                &seeded,
                source_id,
                issue,
                &trace_hex(index + 1),
                now - MINUTE_NS,
                None,
                None,
            )
            .await;
        }

        let condition =
            issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, None, Some(source_id));
        let measured = measure_rule(
            &app.state,
            seeded.org_id,
            project,
            &condition,
            now - 15 * MINUTE_NS,
            now,
        )
        .await
        .expect("the rule measures");
        // Open, resolved, and the target with its merged issue; not the ignored group.
        assert_eq!(measured.count, 4);
    }

    #[tokio::test]
    async fn source_rule_with_no_issues_is_zero() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "empty", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;

        let condition =
            issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, None, Some(source_id));
        let measured = measure_rule(
            &app.state,
            seeded.org_id,
            project,
            &condition,
            now - 15 * MINUTE_NS,
            now,
        )
        .await
        .expect("the rule measures");
        assert_eq!(
            measured,
            AlertValue {
                count: 0,
                value: Some(0.0),
            }
        );
    }

    #[tokio::test]
    async fn zero_occurrences_is_a_value_and_can_breach() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "errors", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        let issue = Uuid::now_v7();
        put_merged_or_plain(&app, project, source_id, issue, IssueState::Open, None, now).await;

        // "Fewer than one occurrence in 15 minutes" over an empty window breaches.
        let condition = issue_condition(AlertWindow::M15, Comparator::Lt, 1.0, Some(issue), None);
        let measured = measure_rule(
            &app.state,
            seeded.org_id,
            project,
            &condition,
            now - 15 * MINUTE_NS,
            now,
        )
        .await
        .expect("the rule measures");
        assert_eq!(measured.count, 0);
        assert_eq!(measured.value, Some(0.0));
        assert_eq!(
            reading(&condition, measured.count, measured.value),
            Reading::Breaching
        );
    }

    #[tokio::test]
    async fn names_over_the_cap_are_chunked() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "bulk", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        // One more issue than a single read may name.
        let ids: Vec<Uuid> = (0..=MAX_COUNT_NAMES).map(|_| Uuid::now_v7()).collect();
        let rows: Vec<IssueRow> = ids
            .iter()
            .map(|id| issue_row(project, source_id, *id, "bulk", IssueState::Open, now))
            .collect();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    for row in &rows {
                        insert_issue(conn, row).await?;
                    }
                    Ok(())
                })
            })
            .await
            .unwrap();
        let first = ids[0];
        let last = ids[MAX_COUNT_NAMES];
        occurrence(
            &app,
            &seeded,
            source_id,
            first,
            &trace_hex(1),
            now - MINUTE_NS,
            None,
            None,
        )
        .await;
        occurrence(
            &app,
            &seeded,
            source_id,
            last,
            &trace_hex(2),
            now - MINUTE_NS,
            None,
            None,
        )
        .await;

        let condition =
            issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, None, Some(source_id));
        let measured = measure_rule(
            &app.state,
            seeded.org_id,
            project,
            &condition,
            now - 15 * MINUTE_NS,
            now,
        )
        .await
        .expect("the rule measures");
        assert_eq!(measured.count, 2);
    }

    #[tokio::test]
    async fn missing_issue_fails_with_a_fixed_reason() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "errors", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        let issue = Uuid::now_v7();
        put_merged_or_plain(&app, project, source_id, issue, IssueState::Open, None, now).await;
        let rule = add_rule(
            &app,
            project,
            "issue",
            issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, Some(issue), None),
            Vec::new(),
        )
        .await;
        // The schema cascade removes the issues with their source.
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    sqlx::query("DELETE FROM issue_sources WHERE id = ?")
                        .bind(source_id.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .await
            .unwrap();

        let from = now - 15 * MINUTE_NS;
        let issue_condition_now =
            issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, Some(issue), None);
        assert_eq!(
            measure_rule(
                &app.state,
                seeded.org_id,
                project,
                &issue_condition_now,
                from,
                now
            )
            .await,
            Err("issue not found")
        );
        let source_condition =
            issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, None, Some(source_id));
        assert_eq!(
            measure_rule(
                &app.state,
                seeded.org_id,
                project,
                &source_condition,
                from,
                now
            )
            .await,
            Err("issue source not found")
        );

        let report = evaluate_once(&app.state, now).await.unwrap();
        assert_eq!(
            report,
            EvaluateReport {
                evaluated: 1,
                failed: 1,
                ..EvaluateReport::default()
            }
        );
        let row = read_rule(&app, project, rule.id).await;
        assert_eq!(row.last_error.as_deref(), Some("issue not found"));
        assert_eq!(row.state, AlertState::Ok);
    }

    #[tokio::test]
    async fn deleting_a_source_leaves_its_rules() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "errors", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        let issue = Uuid::now_v7();
        put_merged_or_plain(&app, project, source_id, issue, IssueState::Open, None, now).await;
        let rule = add_rule(
            &app,
            project,
            "issue",
            issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, Some(issue), None),
            Vec::new(),
        )
        .await;

        let path = format!("/api/v1/projects/{project}/issue-sources/{source_id}");
        let deleted = app
            .request(Method::DELETE, &path)
            .header("cookie", &seeded.cookie)
            .send()
            .await;
        assert_eq!(deleted.status, 204, "{:?}", deleted.body);

        let kept = rule_by_id(app.state.meta.reader(), project, rule.id)
            .await
            .expect("query")
            .expect("the rule is kept");
        assert_eq!(kept.id, rule.id);
        let report = evaluate_once(&app.state, now).await.unwrap();
        assert_eq!(report.failed, 1);
        let row = read_rule(&app, project, rule.id).await;
        assert_eq!(row.last_error.as_deref(), Some("issue not found"));
    }

    #[tokio::test]
    async fn issue_rule_fires_and_resolves() {
        let app = TestApp::new().await;
        let seeded = app.seed().await;
        let project = seeded.project.id;
        let now = app.state.clock.now_ns();
        let source = source_row(project, "errors", SourceKind::Error, true, now);
        let source_id = source.id;
        put_source(&app, source).await;
        let issue = Uuid::now_v7();
        put_merged_or_plain(&app, project, source_id, issue, IssueState::Open, None, now).await;
        let rule = add_rule(
            &app,
            project,
            "issue",
            issue_condition(AlertWindow::M15, Comparator::Gt, 0.0, Some(issue), None),
            Vec::new(),
        )
        .await;
        occurrence(
            &app,
            &seeded,
            source_id,
            issue,
            &trace_hex(1),
            now - MINUTE_NS,
            None,
            None,
        )
        .await;

        let report = evaluate_once(&app.state, now).await.unwrap();
        assert_eq!(
            report,
            EvaluateReport {
                evaluated: 1,
                events: 1,
                ..EvaluateReport::default()
            }
        );
        assert_eq!(
            read_rule(&app, project, rule.id).await.state,
            AlertState::Firing
        );

        // The occurrence leaves the window: the first clear reading. Two are needed to resolve.
        app.state.clock.advance(Duration::from_secs(16 * 60));
        let now = app.state.clock.now_ns();
        assert_eq!(
            evaluate_once(&app.state, now).await.unwrap(),
            EvaluateReport {
                evaluated: 1,
                ..EvaluateReport::default()
            }
        );
        let row = read_rule(&app, project, rule.id).await;
        assert_eq!(row.state, AlertState::Firing);
        assert_eq!(row.clear_streak, 1);

        app.state.clock.advance(Duration::from_secs(60));
        let now = app.state.clock.now_ns();
        assert_eq!(
            evaluate_once(&app.state, now).await.unwrap(),
            EvaluateReport {
                evaluated: 1,
                events: 1,
                ..EvaluateReport::default()
            }
        );
        assert_eq!(
            read_rule(&app, project, rule.id).await.state,
            AlertState::Ok
        );
    }
}
