//! Rule condition defaults and checks (D-K-2).

use cetra_storage::AlertMeasure;

use crate::api::alerts::{
    AlertCondition, AlertMetric, AlertWindow, LatencyTarget, ScoreAggregation,
};
use crate::api::common::FieldError;

/// Longest filter, in characters (the UI's `FILTER_MAX`).
pub(crate) const MAX_FILTER_CHARS: usize = 4096;
/// Longest score name, in characters.
pub(crate) const MAX_SCORE_NAME_CHARS: usize = 200;
/// Largest `min_count`.
pub(crate) const MAX_MIN_COUNT: i64 = 1_000_000;
const NS_PER_MS: f64 = 1_000_000.0;

/// Fills the defaults and checks every field. Errors name the field as
/// `condition.<name>`. Several errors are returned together, in field order.
pub(crate) fn normalize_condition(
    mut condition: AlertCondition,
) -> Result<AlertCondition, Vec<FieldError>> {
    let mut errors = Vec::new();

    // 1. filter
    let mut filter = condition
        .filter
        .take()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    if condition.metric == AlertMetric::IssueOccurrences && filter.is_some() {
        errors.push(error("filter", "does not apply to issue_occurrences"));
        filter = None;
    }
    if filter
        .as_ref()
        .is_some_and(|text| text.chars().count() > MAX_FILTER_CHARS)
    {
        errors.push(error("filter", "at most 4096 characters"));
    }
    condition.filter = filter;

    // 2. threshold. The effective aggregation is needed here, before any error is pushed.
    let aggregation = condition.aggregation.unwrap_or(ScoreAggregation::Avg);
    let threshold = condition.threshold;
    if threshold.is_finite() {
        let problem = match condition.metric {
            AlertMetric::ErrorRate => {
                (!(0.0..=1.0).contains(&threshold)).then_some("must be between 0 and 1")
            }
            AlertMetric::Cost | AlertMetric::IssueOccurrences => {
                (threshold < 0.0).then_some("must be at least 0")
            }
            AlertMetric::LatencyP95 => {
                // A trace is measured from its start (D-K-18), so a trace threshold at or
                // above the window cannot be measured fairly; llm spans are not affected.
                let window_ms = condition.window.ns() as f64 / NS_PER_MS;
                let llm_span = condition.target == Some(LatencyTarget::LlmSpan);
                if threshold <= 0.0 {
                    Some("must be greater than 0")
                } else if !llm_span && threshold >= window_ms {
                    Some("must be less than the window")
                } else {
                    None
                }
            }
            AlertMetric::Score => (aggregation == ScoreAggregation::PassRate
                && !(0.0..=1.0).contains(&threshold))
            .then_some("must be between 0 and 1"),
        };
        if let Some(message) = problem {
            errors.push(error("threshold", message));
        }
    } else {
        errors.push(error("threshold", "must be a finite number"));
    }

    // 3. min_count
    match condition.metric {
        AlertMetric::Cost | AlertMetric::IssueOccurrences => {
            if condition.min_count.is_some() {
                errors.push(error(
                    "min_count",
                    &format!("does not apply to {}", condition.metric.as_str()),
                ));
            }
            condition.min_count = None;
        }
        metric => {
            let default = if metric == AlertMetric::Score { 10 } else { 20 };
            let min_count = condition.min_count.unwrap_or(default);
            if !(1..=MAX_MIN_COUNT).contains(&min_count) {
                errors.push(error("min_count", "must be between 1 and 1000000"));
            }
            condition.min_count = Some(min_count);
        }
    }

    // 4. target
    if condition.metric == AlertMetric::LatencyP95 {
        condition.target = Some(condition.target.unwrap_or(LatencyTarget::Trace));
    } else if condition.target.is_some() {
        errors.push(error("target", "applies to latency_p95 only"));
    }

    // 5. score_name
    if condition.metric == AlertMetric::Score {
        let name = condition
            .score_name
            .take()
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty());
        match name {
            None => errors.push(error("score_name", "is required for score")),
            Some(name) => {
                if name.chars().count() > MAX_SCORE_NAME_CHARS {
                    errors.push(error("score_name", "at most 200 characters"));
                }
                condition.score_name = Some(name);
            }
        }
    } else if condition.score_name.is_some() {
        errors.push(error("score_name", "applies to score only"));
    }

    // 6. aggregation
    if condition.metric == AlertMetric::Score {
        condition.aggregation = Some(aggregation);
    } else if condition.aggregation.is_some() {
        errors.push(error("aggregation", "applies to score only"));
    }

    // 7. window: an occurrence is written after its trace completes (D-IC-20), so a short
    // window would miss it.
    if condition.metric == AlertMetric::IssueOccurrences
        && matches!(condition.window, AlertWindow::M1 | AlertWindow::M5)
    {
        errors.push(error(
            "window",
            "issue occurrences need a window of 15m or more",
        ));
    }

    // 8. issue_id and source_id: exactly one for issue_occurrences, neither for the others.
    if condition.metric == AlertMetric::IssueOccurrences {
        match (condition.issue_id.is_some(), condition.source_id.is_some()) {
            (true, true) => errors.push(error("source_id", "cannot be set with issue_id")),
            (false, false) => errors.push(error(
                "issue_id",
                "one of issue_id and source_id is required",
            )),
            _ => {}
        }
    } else {
        if condition.issue_id.is_some() {
            errors.push(error("issue_id", "applies to issue_occurrences only"));
        }
        if condition.source_id.is_some() {
            errors.push(error("source_id", "applies to issue_occurrences only"));
        }
    }

    if errors.is_empty() {
        Ok(condition)
    } else {
        Err(errors)
    }
}

fn error(field: &str, message: &str) -> FieldError {
    FieldError {
        field: format!("condition.{field}"),
        message: message.to_owned(),
        position: None,
    }
}

/// The storage measure for a normalized condition. `None` for `issue_occurrences`: the
/// evaluator builds that measure from the issue names it resolves (D-IC-20).
pub(crate) fn measure(condition: &AlertCondition) -> Option<AlertMeasure> {
    let name = || condition.score_name.clone().unwrap_or_default();
    match condition.metric {
        AlertMetric::ErrorRate => Some(AlertMeasure::ErrorRate),
        AlertMetric::Cost => Some(AlertMeasure::Cost),
        AlertMetric::LatencyP95 => Some(match condition.target {
            Some(LatencyTarget::LlmSpan) => AlertMeasure::LlmLatencyP95,
            Some(LatencyTarget::Trace) | None => AlertMeasure::TraceLatencyP95,
        }),
        AlertMetric::Score => Some(match condition.aggregation {
            Some(ScoreAggregation::PassRate) => AlertMeasure::ScorePassRate { name: name() },
            Some(ScoreAggregation::Avg) | None => AlertMeasure::ScoreAvg { name: name() },
        }),
        AlertMetric::IssueOccurrences => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::alerts::Comparator;
    use serde_json::json;
    use uuid::Uuid;

    fn condition(metric: AlertMetric) -> AlertCondition {
        AlertCondition {
            metric,
            window: AlertWindow::M5,
            filter: None,
            comparator: Comparator::Gt,
            threshold: 0.5,
            min_count: None,
            target: None,
            score_name: None,
            aggregation: None,
            issue_id: None,
            source_id: None,
        }
    }

    /// The field names of every error, in order; empty when the condition is valid.
    fn fields(condition: AlertCondition) -> Vec<String> {
        match normalize_condition(condition) {
            Ok(_) => Vec::new(),
            Err(errors) => errors.into_iter().map(|error| error.field).collect(),
        }
    }

    #[test]
    fn defaults_are_filled() {
        let rate = normalize_condition(condition(AlertMetric::ErrorRate)).expect("valid");
        assert_eq!(rate.min_count, Some(20));

        let mut latency = condition(AlertMetric::LatencyP95);
        latency.threshold = 100.0;
        let latency = normalize_condition(latency).expect("valid");
        assert_eq!(latency.min_count, Some(20));
        assert_eq!(latency.target, Some(LatencyTarget::Trace));

        let mut score = condition(AlertMetric::Score);
        score.score_name = Some(" helpfulness ".to_owned());
        let score = normalize_condition(score).expect("valid");
        assert_eq!(score.min_count, Some(10));
        assert_eq!(score.aggregation, Some(ScoreAggregation::Avg));
        assert_eq!(score.score_name.as_deref(), Some("helpfulness"));

        let cost = normalize_condition(condition(AlertMetric::Cost)).expect("valid");
        assert_eq!(cost.min_count, None);
    }

    #[test]
    fn filter_is_trimmed_and_empty_is_none() {
        let mut filter = condition(AlertMetric::ErrorRate);
        filter.filter = Some("  error_count > 0 ".to_owned());
        let filter = normalize_condition(filter).expect("valid");
        assert_eq!(filter.filter.as_deref(), Some("error_count > 0"));

        let mut blank = condition(AlertMetric::ErrorRate);
        blank.filter = Some("   ".to_owned());
        let blank = normalize_condition(blank).expect("valid");
        assert_eq!(blank.filter, None);

        let mut long = condition(AlertMetric::ErrorRate);
        long.filter = Some("a".repeat(4097));
        assert_eq!(fields(long), ["condition.filter"]);
    }

    #[test]
    fn thresholds_are_checked_per_metric() {
        let mut rate = condition(AlertMetric::ErrorRate);
        rate.threshold = 1.5;
        assert_eq!(fields(rate), ["condition.threshold"]);

        let mut cost = condition(AlertMetric::Cost);
        cost.threshold = -1.0;
        assert_eq!(fields(cost), ["condition.threshold"]);

        let mut latency = condition(AlertMetric::LatencyP95);
        latency.threshold = 0.0;
        assert_eq!(fields(latency), ["condition.threshold"]);

        let mut pass_rate = condition(AlertMetric::Score);
        pass_rate.score_name = Some("s".to_owned());
        pass_rate.aggregation = Some(ScoreAggregation::PassRate);
        pass_rate.threshold = 1.2;
        assert_eq!(fields(pass_rate), ["condition.threshold"]);

        let mut avg = condition(AlertMetric::Score);
        avg.score_name = Some("s".to_owned());
        avg.threshold = -3.0;
        assert_eq!(fields(avg), Vec::<String>::new());

        let mut nan = condition(AlertMetric::ErrorRate);
        nan.threshold = f64::NAN;
        let errors = normalize_condition(nan).expect_err("NAN is refused");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "condition.threshold");
        assert_eq!(errors[0].message, "must be a finite number");
    }

    #[test]
    fn trace_latency_threshold_must_be_below_the_window() {
        // M5 is 300 000 ms.
        let trace_at = |threshold: f64, target: Option<LatencyTarget>| {
            let mut rule = condition(AlertMetric::LatencyP95);
            rule.threshold = threshold;
            rule.target = target;
            rule
        };
        assert!(normalize_condition(trace_at(299_999.0, None)).is_ok());
        assert_eq!(fields(trace_at(300_000.0, None)), ["condition.threshold"]);
        assert_eq!(
            fields(trace_at(300_000.0, Some(LatencyTarget::Trace))),
            ["condition.threshold"]
        );
        let errors = normalize_condition(trace_at(400_000.0, Some(LatencyTarget::Trace)))
            .expect_err("too long");
        assert_eq!(errors[0].message, "must be less than the window");
        // Span latency is measured over the window itself: no such limit.
        assert!(normalize_condition(trace_at(400_000.0, Some(LatencyTarget::LlmSpan))).is_ok());
    }

    #[test]
    fn fields_that_do_not_apply_are_refused() {
        let mut cost = condition(AlertMetric::Cost);
        cost.min_count = Some(5);
        assert_eq!(fields(cost), ["condition.min_count"]);

        let mut rate = condition(AlertMetric::ErrorRate);
        rate.target = Some(LatencyTarget::LlmSpan);
        assert_eq!(fields(rate), ["condition.target"]);

        let mut named = condition(AlertMetric::ErrorRate);
        named.score_name = Some("x".to_owned());
        assert_eq!(fields(named), ["condition.score_name"]);

        let mut cost_aggregation = condition(AlertMetric::Cost);
        cost_aggregation.aggregation = Some(ScoreAggregation::Avg);
        assert_eq!(fields(cost_aggregation), ["condition.aggregation"]);
    }

    #[test]
    fn score_needs_a_name() {
        let errors = normalize_condition(condition(AlertMetric::Score)).expect_err("no name");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "condition.score_name");
        assert_eq!(errors[0].message, "is required for score");
    }

    #[test]
    fn min_count_range() {
        let with_min = |min_count: i64| {
            let mut rule = condition(AlertMetric::ErrorRate);
            rule.min_count = Some(min_count);
            rule
        };
        assert_eq!(fields(with_min(0)), ["condition.min_count"]);
        assert_eq!(fields(with_min(1_000_001)), ["condition.min_count"]);
        assert!(normalize_condition(with_min(1)).is_ok());
        assert!(normalize_condition(with_min(1_000_000)).is_ok());
    }

    #[test]
    fn errors_are_collected_together() {
        let mut rule = condition(AlertMetric::ErrorRate);
        rule.threshold = 2.0;
        rule.target = Some(LatencyTarget::Trace);
        assert_eq!(fields(rule), ["condition.threshold", "condition.target"]);
    }

    #[test]
    fn condition_json_round_trips() {
        let value = json!({
            "metric": "score",
            "window": "1h",
            "comparator": "lt",
            "threshold": 0.7,
            "score_name": "helpfulness",
            "aggregation": "pass_rate",
            "min_count": 10
        });
        let parsed: AlertCondition = serde_json::from_value(value.clone()).expect("parses");
        assert_eq!(serde_json::to_value(&parsed).expect("serializes"), value);

        let mut with_group = value.clone();
        with_group["group_by"] = json!("model");
        assert!(serde_json::from_value::<AlertCondition>(with_group).is_err());

        let mut two_hours = value;
        two_hours["window"] = json!("2h");
        assert!(serde_json::from_value::<AlertCondition>(two_hours).is_err());
    }

    #[test]
    fn window_interval() {
        assert_eq!(AlertMetric::LatencyP95.as_str(), "latency_p95");
        assert_eq!(AlertWindow::M15.interval_ns(), 60_000_000_000);
        assert_eq!(AlertWindow::H1.interval_ns(), 300_000_000_000);
        assert_eq!(AlertWindow::H24.ns(), 86_400_000_000_000);
        assert_eq!(AlertWindow::M5.as_str(), "5m");
    }

    #[test]
    fn comparator_breaches() {
        assert!(!Comparator::Gt.breaches(1.0, 1.0));
        assert!(Comparator::Gt.breaches(1.1, 1.0));
        assert!(Comparator::Gte.breaches(1.0, 1.0));
        assert!(Comparator::Lt.breaches(0.5, 1.0));
        assert!(!Comparator::Lt.breaches(1.0, 1.0));
        assert!(Comparator::Lte.breaches(1.0, 1.0));
        assert!(!Comparator::Lte.breaches(1.1, 1.0));
    }

    #[test]
    fn measure_follows_the_condition() {
        assert_eq!(
            measure(&condition(AlertMetric::ErrorRate)),
            Some(AlertMeasure::ErrorRate)
        );
        assert_eq!(
            measure(&condition(AlertMetric::Cost)),
            Some(AlertMeasure::Cost)
        );
        let mut latency = condition(AlertMetric::LatencyP95);
        assert_eq!(measure(&latency), Some(AlertMeasure::TraceLatencyP95));
        latency.target = Some(LatencyTarget::Trace);
        assert_eq!(measure(&latency), Some(AlertMeasure::TraceLatencyP95));
        latency.target = Some(LatencyTarget::LlmSpan);
        assert_eq!(measure(&latency), Some(AlertMeasure::LlmLatencyP95));
        let mut score = condition(AlertMetric::Score);
        score.score_name = Some("helpfulness".to_owned());
        let name = "helpfulness".to_owned();
        assert_eq!(
            measure(&score),
            Some(AlertMeasure::ScoreAvg { name: name.clone() })
        );
        score.aggregation = Some(ScoreAggregation::Avg);
        assert_eq!(
            measure(&score),
            Some(AlertMeasure::ScoreAvg { name: name.clone() })
        );
        score.aggregation = Some(ScoreAggregation::PassRate);
        assert_eq!(measure(&score), Some(AlertMeasure::ScorePassRate { name }));
    }

    #[test]
    fn issue_occurrences_has_no_storage_measure() {
        let mut issues = condition(AlertMetric::IssueOccurrences);
        issues.window = AlertWindow::M15;
        issues.issue_id = Some(Uuid::now_v7());
        assert_eq!(measure(&issues), None);
    }

    /// An `issue_occurrences` condition over `issue_id`, 15 minutes, `gte` 2.5.
    fn issue_rule() -> AlertCondition {
        AlertCondition {
            metric: AlertMetric::IssueOccurrences,
            window: AlertWindow::M15,
            filter: None,
            comparator: Comparator::Gte,
            threshold: 2.5,
            min_count: None,
            target: None,
            score_name: None,
            aggregation: None,
            issue_id: Some(Uuid::now_v7()),
            source_id: None,
        }
    }

    #[test]
    fn issue_occurrences_refuses_short_windows() {
        let mut one_minute = issue_rule();
        one_minute.window = AlertWindow::M1;
        assert_eq!(fields(one_minute), ["condition.window"]);
        let errors = normalize_condition({
            let mut five = issue_rule();
            five.window = AlertWindow::M5;
            five
        })
        .expect_err("5m is refused");
        assert_eq!(
            errors[0].message,
            "issue occurrences need a window of 15m or more"
        );

        for window in [
            AlertWindow::M15,
            AlertWindow::H1,
            AlertWindow::H6,
            AlertWindow::H24,
        ] {
            let mut rule = issue_rule();
            rule.window = window;
            assert!(normalize_condition(rule).is_ok());
        }

        let mut rate = condition(AlertMetric::ErrorRate);
        rate.window = AlertWindow::M1;
        assert!(normalize_condition(rate).is_ok());
    }

    #[test]
    fn issue_occurrences_threshold_is_checked() {
        let with_threshold = |threshold: f64| {
            let mut rule = issue_rule();
            rule.threshold = threshold;
            rule
        };
        let errors = normalize_condition(with_threshold(-1.0)).expect_err("negative");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "condition.threshold");
        assert_eq!(errors[0].message, "must be at least 0");

        for threshold in [0.0, 3.0, 2.5] {
            assert!(normalize_condition(with_threshold(threshold)).is_ok());
        }

        let errors = normalize_condition(with_threshold(f64::NAN)).expect_err("NaN");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "must be a finite number");
    }

    #[test]
    fn issue_occurrences_refuses_fields_that_do_not_apply() {
        let mut rule = issue_rule();
        rule.filter = Some("error_count > 0".to_owned());
        rule.min_count = Some(5);
        rule.target = Some(LatencyTarget::Trace);
        rule.score_name = Some("helpfulness".to_owned());
        rule.aggregation = Some(ScoreAggregation::Avg);
        assert_eq!(
            fields(rule.clone()),
            [
                "condition.filter",
                "condition.min_count",
                "condition.target",
                "condition.score_name",
                "condition.aggregation",
            ]
        );
        let errors = normalize_condition(rule).expect_err("refused");
        assert_eq!(errors[0].message, "does not apply to issue_occurrences");
        assert_eq!(errors[1].message, "does not apply to issue_occurrences");
    }

    #[test]
    fn issue_occurrences_needs_exactly_one_scope() {
        let mut neither = issue_rule();
        neither.issue_id = None;
        let errors = normalize_condition(neither).expect_err("no scope");
        assert_eq!(errors[0].field, "condition.issue_id");
        assert_eq!(
            errors[0].message,
            "one of issue_id and source_id is required"
        );

        let both = issue_rule_with_scope(Some(Uuid::now_v7()), Some(Uuid::now_v7()));
        let errors = normalize_condition(both).expect_err("two scopes");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "condition.source_id");
        assert_eq!(errors[0].message, "cannot be set with issue_id");

        let by_issue = issue_rule_with_scope(Some(Uuid::now_v7()), None);
        let kept = normalize_condition(by_issue).expect("one scope passes");
        assert!(kept.issue_id.is_some());
        assert_eq!(kept.source_id, None);

        let source = Uuid::now_v7();
        let by_source = issue_rule_with_scope(None, Some(source));
        let kept = normalize_condition(by_source).expect("one scope passes");
        assert_eq!(kept.source_id, Some(source));
        assert_eq!(kept.issue_id, None);
    }

    /// `issue_rule()` with the scope fields replaced.
    fn issue_rule_with_scope(issue_id: Option<Uuid>, source_id: Option<Uuid>) -> AlertCondition {
        AlertCondition {
            issue_id,
            source_id,
            ..issue_rule()
        }
    }

    #[test]
    fn other_metrics_refuse_issue_and_source_ids() {
        let mut by_issue = condition(AlertMetric::ErrorRate);
        by_issue.issue_id = Some(Uuid::now_v7());
        let errors = normalize_condition(by_issue).expect_err("refused");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "condition.issue_id");
        assert_eq!(errors[0].message, "applies to issue_occurrences only");

        let mut by_source = condition(AlertMetric::Cost);
        by_source.source_id = Some(Uuid::now_v7());
        let errors = normalize_condition(by_source).expect_err("refused");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "condition.source_id");
        assert_eq!(errors[0].message, "applies to issue_occurrences only");
    }

    #[test]
    fn issue_occurrences_json_round_trips() {
        let issue = "0190a7f0-0000-7000-8000-000000000042";
        let value = json!({
            "metric": "issue_occurrences",
            "window": "15m",
            "comparator": "gte",
            "threshold": 2.5,
            "issue_id": issue,
        });
        let parsed: AlertCondition = serde_json::from_value(value.clone()).expect("parses");
        assert_eq!(
            parsed.issue_id,
            Some(issue.parse::<Uuid>().expect("a uuid"))
        );
        assert_eq!(serde_json::to_value(&parsed).expect("serializes"), value);

        let mut with_unknown = value.clone();
        with_unknown["bogus"] = json!(1);
        assert!(serde_json::from_value::<AlertCondition>(with_unknown).is_err());

        let mut bad_id = value;
        bad_id["issue_id"] = json!("not-a-uuid");
        assert!(serde_json::from_value::<AlertCondition>(bad_id).is_err());
    }
}
