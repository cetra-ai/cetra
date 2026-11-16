//! The checks between an online rule and its evaluator (spec 06 § Online rules, D-I-19,
//! D-I-30). Pure: the routes read the rows and pass them in.

use std::collections::BTreeSet;

use cetra_eval::{Adapter, Check, EvaluatorConfig, JudgeConfig, PromptTemplate, VariableMapping};
use cetra_otel::PriceTable;

use crate::api::common::FieldError;
use crate::api::evaluators::EvaluatorKind;
use crate::api::online_rules::RuleTarget;
use crate::eval::budget::model_price;
use crate::eval::evaluator::EvaluatorError;
use crate::meta::evaluators::EvaluatorRow;

/// What the rule checks need of an evaluator. Built from the stored row without opening a
/// webhook's sealed values (no check needs the URL, headers or secret).
pub(crate) enum Subject {
    /// An LLM judge.
    Judge(JudgeConfig),
    /// A built-in check.
    Check(Check),
    /// A webhook. Its rules have no variable checks and no budget.
    Webhook,
}

impl Subject {
    /// From a stored row: `kind` through `EvaluatorKind::from_name`; a judge's or check's `config`
    /// through `EvaluatorConfig::from_parts(kind, &document)` as `evaluator::load` does; a webhook
    /// is `Webhook` without reading `secret_enc`.
    ///
    /// # Errors
    ///
    /// [`EvaluatorError::Corrupt`] when a column does not decode.
    pub(crate) fn from_row(row: &EvaluatorRow) -> Result<Self, EvaluatorError> {
        let kind = EvaluatorKind::from_name(&row.kind).ok_or(EvaluatorError::Corrupt("kind"))?;
        match kind {
            EvaluatorKind::Webhook => Ok(Self::Webhook),
            EvaluatorKind::LlmJudge | EvaluatorKind::Check => {
                let document: serde_json::Value = serde_json::from_str(&row.config)
                    .map_err(|_| EvaluatorError::Corrupt("config"))?;
                let config = EvaluatorConfig::from_parts(kind.as_str(), &document)
                    .map_err(|_| EvaluatorError::Corrupt("config"))?;
                Ok(Self::from_config(&config))
            }
        }
    }

    /// From a parsed configuration (the evaluator PATCH's new value).
    pub(crate) fn from_config(config: &EvaluatorConfig) -> Self {
        match config {
            EvaluatorConfig::LlmJudge(judge) => Self::Judge(judge.clone()),
            EvaluatorConfig::Check(check) => Self::Check(check.clone()),
            EvaluatorConfig::Webhook(_) => Self::Webhook,
        }
    }
}

/// What a rule asks of its evaluator (§ Rule validation, checks 8–10, 14–15).
pub(crate) struct RuleShape<'a> {
    /// The rule's target.
    pub(crate) target: RuleTarget,
    /// The parsed mapping, or `None` when it does not parse (its errors are the caller's). Only
    /// the checks that need a parsed mapping are skipped then.
    pub(crate) mapping: Option<&'a VariableMapping>,
    /// The mapping's variable names.
    pub(crate) variables: &'a BTreeSet<String>,
    /// Whether the rule has a budget.
    pub(crate) budgeted: bool,
}

/// Every check of the rule against `subject`, as field errors (§ Rule validation, checks 8–10,
/// 14–15). `adapter` is the judge's connection adapter (`None` for other kinds, or when the
/// connection is gone, which fails check 15). `prices` and `now` price the judge's model.
pub(crate) fn check_rule(
    shape: &RuleShape<'_>,
    subject: &Subject,
    adapter: Option<Adapter>,
    prices: &PriceTable,
    now: i64,
) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if let Some(mapping) = shape.mapping
        && shape.target == RuleTarget::Trace
        && mapping.needs_target_span()
    {
        errors.push(field_error(
            "variable_mapping",
            "span.* needs a span rule; use a selector such as span[name=…]",
        ));
    }
    match subject {
        Subject::Judge(judge) => errors.extend(unmapped_variables(judge, shape.variables)),
        Subject::Check(check) => {
            if !shape.variables.contains("output") {
                errors.push(field_error(
                    "variable_mapping.output",
                    "the check needs the output variable",
                ));
            }
            if needs_expected(check) && !shape.variables.contains("expected") {
                errors.push(field_error(
                    "variable_mapping.expected",
                    "the check needs the expected variable",
                ));
            }
        }
        Subject::Webhook => {}
    }
    if shape.budgeted {
        match subject {
            Subject::Judge(judge) => {
                let priced = adapter.is_some_and(|adapter| {
                    model_price(prices, &judge.model, adapter, now).is_some()
                });
                if !priced {
                    errors.push(field_error(
                        "budget_usd",
                        "the judge model has no price, so the rule cannot have a budget; add one under Settings → Model prices",
                    ));
                }
            }
            Subject::Check(_) | Subject::Webhook => errors.push(field_error(
                "budget_usd",
                "budgets apply to LLM judge evaluators only",
            )),
        }
    }
    errors
}

/// Check 9 alone: one error per variable of the judge's prompt that `variables` lacks. A prompt
/// that no longer compiles gives one error on `variable_mapping`.
pub(crate) fn unmapped_variables(
    judge: &JudgeConfig,
    variables: &BTreeSet<String>,
) -> Vec<FieldError> {
    match PromptTemplate::compile(&judge.prompt) {
        Ok(template) => template
            .variables()
            .into_iter()
            .filter(|name| !variables.contains(name))
            .map(|name| FieldError {
                field: format!("variable_mapping.{name}"),
                message: format!("variable {name} is used by the prompt but not mapped"),
                position: None,
            })
            .collect(),
        Err(error) => vec![field_error("variable_mapping", &error.to_string())],
    }
}

/// Whether a check reads `expected` (it compares with it, or has no needles of its own).
fn needs_expected(check: &Check) -> bool {
    match check {
        Check::ExactMatch { .. } | Check::Levenshtein { .. } => true,
        Check::Contains { values, .. } => values.is_empty(),
        Check::Regex { .. } | Check::JsonValid | Check::JsonSchema { .. } => false,
    }
}

pub(crate) fn field_error(field: &str, message: &str) -> FieldError {
    FieldError {
        field: field.to_owned(),
        message: message.to_owned(),
        position: None,
    }
}

#[cfg(test)]
mod tests {
    use cetra_eval::JudgeOutput;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    fn judge(prompt: &str, model: &str) -> JudgeConfig {
        JudgeConfig {
            connection_id: Uuid::from_u128(4),
            model: model.to_owned(),
            prompt: prompt.to_owned(),
            output: JudgeOutput::Boolean,
            include_reason: false,
            temperature: None,
            max_output_tokens: 1024,
            timeout_ms: 60_000,
        }
    }

    fn mapping(pairs: &[(&str, &str)]) -> VariableMapping {
        let value = Value::Object(
            pairs
                .iter()
                .map(|(name, expr)| ((*name).to_owned(), Value::String((*expr).to_owned())))
                .collect(),
        );
        VariableMapping::from_json(&value).expect("a valid mapping")
    }

    fn names(pairs: &[(&str, &str)]) -> BTreeSet<String> {
        pairs.iter().map(|(name, _)| (*name).to_owned()).collect()
    }

    fn check_errors(
        target: RuleTarget,
        pairs: &[(&str, &str)],
        subject: &Subject,
        budgeted: bool,
        adapter: Option<Adapter>,
    ) -> Vec<(String, String)> {
        let mapping = mapping(pairs);
        let variables = names(pairs);
        let shape = RuleShape {
            target,
            mapping: Some(&mapping),
            variables: &variables,
            budgeted,
        };
        let prices = PriceTable::new(Vec::new()).expect("an empty table");
        check_rule(&shape, subject, adapter, &prices, 0)
            .into_iter()
            .map(|error| (error.field, error.message))
            .collect()
    }

    fn pair(field: &str, message: &str) -> (String, String) {
        (field.to_owned(), message.to_owned())
    }

    #[test]
    fn a_trace_rule_cannot_read_a_bare_span() {
        let errors = check_errors(
            RuleTarget::Trace,
            &[("output", "span.output")],
            &Subject::Webhook,
            false,
            None,
        );
        assert_eq!(
            errors,
            [pair(
                "variable_mapping",
                "span.* needs a span rule; use a selector such as span[name=…]"
            )]
        );
        let clean = check_errors(
            RuleTarget::Span,
            &[("output", "span.output")],
            &Subject::Webhook,
            false,
            None,
        );
        assert!(clean.is_empty(), "{clean:?}");
    }

    #[test]
    fn a_judge_needs_every_prompt_variable_mapped() {
        let subject = Subject::Judge(judge("Q: {{ input }} A: {{ output }}", "m"));
        let clean = check_errors(
            RuleTarget::Trace,
            &[("input", "trace.input"), ("output", "trace.output")],
            &subject,
            false,
            Some(Adapter::OpenAi),
        );
        assert!(clean.is_empty(), "{clean:?}");
        let errors = check_errors(
            RuleTarget::Trace,
            &[("output", "trace.output")],
            &subject,
            false,
            Some(Adapter::OpenAi),
        );
        assert_eq!(
            errors,
            [pair(
                "variable_mapping.input",
                "variable input is used by the prompt but not mapped"
            )]
        );
    }

    #[test]
    fn unmapped_variables_reports_each_missing_name_alone() {
        let judge = judge("{{ input }} {{ output }} {{ context }}", "m");
        let variables = names(&[("input", ""), ("output", "")]);
        let errors = unmapped_variables(&judge, &variables);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].field, "variable_mapping.context");
        assert_eq!(
            errors[0].message,
            "variable context is used by the prompt but not mapped"
        );
    }

    #[test]
    fn the_key_checks_run_without_a_parsed_mapping() {
        // The mapping did not parse, so only the checks on its keys run: a `span.*` check is skipped.
        let judge = judge("{{ input }} {{ output }}", "m");
        let variables = names(&[("input", "")]);
        let shape = RuleShape {
            target: RuleTarget::Trace,
            mapping: None,
            variables: &variables,
            budgeted: false,
        };
        let prices = PriceTable::new(Vec::new()).expect("an empty table");
        let errors = check_rule(
            &shape,
            &Subject::Judge(judge),
            Some(Adapter::OpenAi),
            &prices,
            0,
        );
        assert_eq!(
            errors
                .iter()
                .map(|error| error.field.as_str())
                .collect::<Vec<_>>(),
            ["variable_mapping.output"]
        );
    }

    #[test]
    fn a_broken_prompt_is_one_error_on_the_mapping() {
        let judge = judge("{{ input ", "m");
        let errors = unmapped_variables(&judge, &BTreeSet::new());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "variable_mapping");
        assert_eq!(
            errors[0].message,
            PromptTemplate::compile(&judge.prompt)
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default()
        );
    }

    #[test]
    fn a_check_needs_output_and_the_checks_that_compare_need_expected() {
        let exact = Subject::Check(Check::ExactMatch {
            case_sensitive: true,
            trim: true,
        });
        let errors = check_errors(
            RuleTarget::Trace,
            &[("input", "trace.input")],
            &exact,
            false,
            None,
        );
        assert_eq!(
            errors,
            [
                pair(
                    "variable_mapping.output",
                    "the check needs the output variable"
                ),
                pair(
                    "variable_mapping.expected",
                    "the check needs the expected variable"
                ),
            ]
        );
        let levenshtein = Subject::Check(Check::Levenshtein { normalize: true });
        let errors = check_errors(
            RuleTarget::Trace,
            &[("output", "trace.output")],
            &levenshtein,
            false,
            None,
        );
        assert_eq!(
            errors,
            [pair(
                "variable_mapping.expected",
                "the check needs the expected variable"
            )]
        );
    }

    #[test]
    fn a_contains_check_needs_expected_only_without_needles() {
        let with_needles = Subject::Check(Check::Contains {
            value: None,
            values: vec!["refund".to_owned()],
            mode: cetra_eval::ContainsMode::Any,
            case_sensitive: false,
        });
        let without = Subject::Check(Check::Contains {
            value: None,
            values: Vec::new(),
            mode: cetra_eval::ContainsMode::Any,
            case_sensitive: false,
        });
        let mapped = [("output", "trace.output")];
        assert!(check_errors(RuleTarget::Trace, &mapped, &with_needles, false, None).is_empty());
        assert_eq!(
            check_errors(RuleTarget::Trace, &mapped, &without, false, None),
            [pair(
                "variable_mapping.expected",
                "the check needs the expected variable"
            )]
        );
        let json_valid = Subject::Check(Check::JsonValid);
        assert!(check_errors(RuleTarget::Trace, &mapped, &json_valid, false, None).is_empty());
    }

    #[test]
    fn a_budget_needs_a_judge_and_a_priced_model() {
        let errors = check_errors(
            RuleTarget::Trace,
            &[("output", "trace.output")],
            &Subject::Check(Check::JsonValid),
            true,
            None,
        );
        assert_eq!(
            errors,
            [pair(
                "budget_usd",
                "budgets apply to LLM judge evaluators only"
            )]
        );
        // No price row for the model: check 15.
        let errors = check_errors(
            RuleTarget::Trace,
            &[("input", "trace.input"), ("output", "trace.output")],
            &Subject::Judge(judge("{{ input }} {{ output }}", "unpriced")),
            true,
            Some(Adapter::OpenAi),
        );
        assert_eq!(
            errors,
            [pair(
                "budget_usd",
                "the judge model has no price, so the rule cannot have a budget; add one under Settings → Model prices"
            )]
        );
    }

    #[test]
    fn a_judge_whose_connection_is_gone_has_no_price() {
        let errors = check_errors(
            RuleTarget::Trace,
            &[("input", "trace.input"), ("output", "trace.output")],
            &Subject::Judge(judge("{{ input }} {{ output }}", "gpt")),
            true,
            None,
        );
        assert_eq!(
            errors,
            [pair(
                "budget_usd",
                "the judge model has no price, so the rule cannot have a budget; add one under Settings → Model prices"
            )]
        );
    }

    #[test]
    fn a_webhook_row_is_read_without_its_secret() {
        let row = EvaluatorRow {
            id: Uuid::from_u128(9),
            project_id: cetra_core::ids::ProjectId::from_uuid(Uuid::from_u128(8)),
            name: "hook".to_owned(),
            kind: "webhook".to_owned(),
            config:
                json!({"url_host": "hooks.example.com", "timeout_ms": 10_000, "header_names": []})
                    .to_string(),
            output_score: json!({"name": "hook", "data_type": "numeric", "categories": null})
                .to_string(),
            connection_id: None,
            secret_enc: Some(vec![0xde, 0xad, 0xbe, 0xef]),
            version: 1,
            created_by: None,
            created_at: 0,
            updated_at: 0,
        };
        assert!(matches!(Subject::from_row(&row), Ok(Subject::Webhook)));
    }
}
