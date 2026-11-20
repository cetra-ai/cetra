//! Branded attribute names, reserved keys, and reserved values.
//!
//! Renaming the product means changing [`cetra_name`](crate::cetra_name). Specs 02, 06, and 10.

/// Product name used in every branded identifier.
///
/// Renaming the product means changing this macro.
#[macro_export]
macro_rules! cetra_name {
    () => {
        "cetra"
    };
    ($suffix:literal) => {
        concat!("cetra", $suffix)
    };
}

/// The product name. Equal to [`cetra_name`](crate::cetra_name).
pub const PRODUCT: &str = crate::cetra_name!();

/// Span attributes our SDKs emit and ingestion reads.
///
/// Spec 02 § Mapping tables and spec 10.
pub mod attr {
    /// `cetra.span.kind`. Spec 02 § Span kind, spec 10.
    pub const SPAN_KIND: &str = crate::cetra_name!(".span.kind");
    /// `cetra.input`. Spec 02 § Input / output, spec 10.
    pub const INPUT: &str = crate::cetra_name!(".input");
    /// `cetra.input.format`. Spec 02 § Input / output.
    pub const INPUT_FORMAT: &str = crate::cetra_name!(".input.format");
    /// `cetra.output`. Spec 02 § Input / output, spec 10.
    pub const OUTPUT: &str = crate::cetra_name!(".output");
    /// `cetra.output.format`. Spec 02 § Input / output.
    pub const OUTPUT_FORMAT: &str = crate::cetra_name!(".output.format");
    /// `cetra.session.id`. Spec 02 § Context fields, spec 10.
    pub const SESSION_ID: &str = crate::cetra_name!(".session.id");
    /// `cetra.user.id`. Spec 02 § Context fields, spec 10.
    pub const USER_ID: &str = crate::cetra_name!(".user.id");
    /// `cetra.tags`. Spec 02 § Context fields, spec 10.
    pub const TAGS: &str = crate::cetra_name!(".tags");
    /// `cetra.metadata.` prefix. One attribute per metadata key. Spec 02 § Context fields.
    pub const METADATA_PREFIX: &str = crate::cetra_name!(".metadata.");
    /// `cetra.environment`. Spec 02 § Context fields, spec 10.
    pub const ENVIRONMENT: &str = crate::cetra_name!(".environment");
    /// `cetra.prompt.name`. The registry prompt a span used (D-V-15).
    pub const PROMPT_NAME: &str = crate::cetra_name!(".prompt.name");
    /// `cetra.prompt.version`. The version of that prompt (D-V-15).
    pub const PROMPT_VERSION: &str = crate::cetra_name!(".prompt.version");
    /// `cetra.release`. Spec 02 § Context fields.
    pub const RELEASE: &str = crate::cetra_name!(".release");
    /// `cetra.ttft_ns`. Spec 02 § Model, provider, usage.
    pub const TTFT_NS: &str = crate::cetra_name!(".ttft_ns");
    /// `cetra.cost.total`. Spec 02 § Cost computation.
    pub const COST_TOTAL: &str = crate::cetra_name!(".cost.total");
    /// `cetra.experiment.id`. Spec 02 § Context fields.
    pub const EXPERIMENT_ID: &str = crate::cetra_name!(".experiment.id");
    /// `cetra.dataset_item.id`. Spec 02 § Context fields.
    pub const DATASET_ITEM_ID: &str = crate::cetra_name!(".dataset_item.id");
}

/// Reserved keys inside the span `metadata` map, plus the normalize-error attribute.
///
/// Spec 02 § Context fields and spec 06 § Evaluators.
pub mod metadata_key {
    /// `cetra.experiment_id`. Spec 02 § Context fields, spec 06.
    pub const EXPERIMENT_ID: &str = crate::cetra_name!(".experiment_id");
    /// `cetra.dataset_item_id`. Spec 02 § Context fields, spec 06.
    pub const DATASET_ITEM_ID: &str = crate::cetra_name!(".dataset_item_id");
    /// `cetra.eval.target_trace_id`. Spec 06 § Evaluators.
    pub const EVAL_TARGET_TRACE_ID: &str = crate::cetra_name!(".eval.target_trace_id");
    /// `cetra.eval.target_span_id`. Spec 06 § Evaluators.
    pub const EVAL_TARGET_SPAN_ID: &str = crate::cetra_name!(".eval.target_span_id");
    /// `cetra.eval.rule_id`. Spec 06 § Evaluators.
    pub const EVAL_RULE_ID: &str = crate::cetra_name!(".eval.rule_id");
    /// `cetra.eval.experiment_id`. Spec 06 § Evaluators.
    pub const EVAL_EXPERIMENT_ID: &str = crate::cetra_name!(".eval.experiment_id");
    /// `cetra.eval.evaluator_id`. Spec 06 § Evaluators.
    pub const EVAL_EVALUATOR_ID: &str = crate::cetra_name!(".eval.evaluator_id");
    /// `cetra.normalize_error`, stored in `attributes`. Spec 02 § Message conversion.
    pub const NORMALIZE_ERROR: &str = crate::cetra_name!(".normalize_error");
    /// `cetra.normalize_warning`, stored in `attributes`: a JSON array of warning codes,
    /// sorted and without duplicates. Spec 02 § Normalization rules.
    pub const NORMALIZE_WARNING: &str = crate::cetra_name!(".normalize_warning");
    /// `cetra.playground.run_by`: the user who ran a playground run (D-V-22).
    pub const PLAYGROUND_RUN_BY: &str = crate::cetra_name!(".playground.run_by");
    /// `cetra.playground.source`: the prompt or span a playground run came from (D-V-22).
    pub const PLAYGROUND_SOURCE: &str = crate::cetra_name!(".playground.source");
}

/// Keys written into score metadata by evaluators.
///
/// Spec 06 § Evaluators. These are not product-branded.
pub mod score_metadata_key {
    /// `judge.trace_id`. Spec 06 § Evaluators.
    pub const JUDGE_TRACE_ID: &str = "judge.trace_id";
    /// `judge.model`. Spec 06 § Evaluators.
    pub const JUDGE_MODEL: &str = "judge.model";
    /// `judge.input_tokens`. Spec 06 § Evaluators.
    pub const JUDGE_INPUT_TOKENS: &str = "judge.input_tokens";
    /// `judge.output_tokens`. Spec 06 § Evaluators.
    pub const JUDGE_OUTPUT_TOKENS: &str = "judge.output_tokens";
    /// `judge.cost_usd`. Spec 06 § Evaluators.
    pub const JUDGE_COST_USD: &str = "judge.cost_usd";
    /// `judge.latency_ms`. Spec 06 § Evaluators.
    pub const JUDGE_LATENCY_MS: &str = "judge.latency_ms";
}

/// Reserved `environment` values.
///
/// Spec 01 `environment` and spec 06 § Evaluators.
pub mod environment {
    /// Evaluator executions. Ingestion rewrites external claims of this value.
    pub const EVAL: &str = crate::cetra_name!("-eval");
    /// What external data that claimed [`EVAL`] is rewritten to. Spec 06.
    pub const EVAL_EXTERNAL: &str = crate::cetra_name!("-eval-external");
    /// Reserved for playground runs (D-V-22, D-V-33).
    pub const PLAYGROUND: &str = crate::cetra_name!("-playground");
    /// What external data that claimed [`PLAYGROUND`] is rewritten to (D-V-33).
    pub const PLAYGROUND_EXTERNAL: &str = crate::cetra_name!("-playground-external");
    /// Environments only the server writes. Their traces are hidden by default, excluded from
    /// online rules, refused as evaluation targets, rewritten when received over OTLP, and not
    /// billed (D-V-33, D-V-34). A new reserved environment is added here.
    pub const RESERVED: &[&str] = &[EVAL, PLAYGROUND];
    /// Default environment for experiment runs. Spec 01 `environment`.
    pub const EXPERIMENT: &str = "experiment";

    /// Whether `environment` is reserved (D-V-33).
    #[must_use]
    pub fn is_reserved(environment: &str) -> bool {
        RESERVED.contains(&environment)
    }

    /// The name that external data claiming a reserved `environment` is rewritten to (D-V-33).
    /// `None` when `environment` is not reserved.
    #[must_use]
    pub fn external_alias(environment: &str) -> Option<&'static str> {
        match environment {
            e if e == EVAL => Some(EVAL_EXTERNAL),
            e if e == PLAYGROUND => Some(PLAYGROUND_EXTERNAL),
            _ => None,
        }
    }
}

/// Media references stored in payloads.
///
/// Spec 01 § Media blob and spec 02 § Media extraction.
pub mod media {
    /// URI scheme for an extracted blob: `media://<sha256>`.
    pub const URI_SCHEME: &str = "media://";
    /// URI written when a blob is dropped for size. Spec 02 § Media extraction.
    pub const DROPPED_URI: &str = crate::cetra_name!(":dropped");
}

/// Prefixes of truncation placeholders. Ingestion formats the full text.
///
/// Spec 02 § Truncation.
pub mod marker {
    /// Prefix of a truncated string: `…[cetra: truncated, original `, then `<N> bytes]`.
    pub const TRUNCATED_STRING_SUFFIX_PREFIX: &str =
        concat!("…[", crate::cetra_name!(), ": truncated, original ");
    /// Prefix of a truncated message placeholder: `[cetra: `, then `<K> messages truncated]`.
    pub const TRUNCATED_MESSAGES_PREFIX: &str = concat!("[", crate::cetra_name!(), ": ");
    /// Replaces an attribute value nested deeper than the ingest depth limit:
    /// `[cetra: max depth]`. Spec 02 § Normalization rules.
    pub const MAX_DEPTH: &str = concat!("[", crate::cetra_name!(), ": max depth]");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> String {
        let rows: &[(&str, &str, &str)] = &[
            ("attr", "SPAN_KIND", attr::SPAN_KIND),
            ("attr", "INPUT", attr::INPUT),
            ("attr", "INPUT_FORMAT", attr::INPUT_FORMAT),
            ("attr", "OUTPUT", attr::OUTPUT),
            ("attr", "OUTPUT_FORMAT", attr::OUTPUT_FORMAT),
            ("attr", "SESSION_ID", attr::SESSION_ID),
            ("attr", "USER_ID", attr::USER_ID),
            ("attr", "TAGS", attr::TAGS),
            ("attr", "METADATA_PREFIX", attr::METADATA_PREFIX),
            ("attr", "ENVIRONMENT", attr::ENVIRONMENT),
            ("attr", "PROMPT_NAME", attr::PROMPT_NAME),
            ("attr", "PROMPT_VERSION", attr::PROMPT_VERSION),
            ("attr", "RELEASE", attr::RELEASE),
            ("attr", "TTFT_NS", attr::TTFT_NS),
            ("attr", "COST_TOTAL", attr::COST_TOTAL),
            ("attr", "EXPERIMENT_ID", attr::EXPERIMENT_ID),
            ("attr", "DATASET_ITEM_ID", attr::DATASET_ITEM_ID),
            ("metadata_key", "EXPERIMENT_ID", metadata_key::EXPERIMENT_ID),
            (
                "metadata_key",
                "DATASET_ITEM_ID",
                metadata_key::DATASET_ITEM_ID,
            ),
            (
                "metadata_key",
                "EVAL_TARGET_TRACE_ID",
                metadata_key::EVAL_TARGET_TRACE_ID,
            ),
            (
                "metadata_key",
                "EVAL_TARGET_SPAN_ID",
                metadata_key::EVAL_TARGET_SPAN_ID,
            ),
            ("metadata_key", "EVAL_RULE_ID", metadata_key::EVAL_RULE_ID),
            (
                "metadata_key",
                "EVAL_EXPERIMENT_ID",
                metadata_key::EVAL_EXPERIMENT_ID,
            ),
            (
                "metadata_key",
                "EVAL_EVALUATOR_ID",
                metadata_key::EVAL_EVALUATOR_ID,
            ),
            (
                "metadata_key",
                "NORMALIZE_ERROR",
                metadata_key::NORMALIZE_ERROR,
            ),
            (
                "metadata_key",
                "NORMALIZE_WARNING",
                metadata_key::NORMALIZE_WARNING,
            ),
            (
                "metadata_key",
                "PLAYGROUND_RUN_BY",
                metadata_key::PLAYGROUND_RUN_BY,
            ),
            (
                "metadata_key",
                "PLAYGROUND_SOURCE",
                metadata_key::PLAYGROUND_SOURCE,
            ),
            (
                "score_metadata_key",
                "JUDGE_TRACE_ID",
                score_metadata_key::JUDGE_TRACE_ID,
            ),
            (
                "score_metadata_key",
                "JUDGE_MODEL",
                score_metadata_key::JUDGE_MODEL,
            ),
            (
                "score_metadata_key",
                "JUDGE_INPUT_TOKENS",
                score_metadata_key::JUDGE_INPUT_TOKENS,
            ),
            (
                "score_metadata_key",
                "JUDGE_OUTPUT_TOKENS",
                score_metadata_key::JUDGE_OUTPUT_TOKENS,
            ),
            (
                "score_metadata_key",
                "JUDGE_COST_USD",
                score_metadata_key::JUDGE_COST_USD,
            ),
            (
                "score_metadata_key",
                "JUDGE_LATENCY_MS",
                score_metadata_key::JUDGE_LATENCY_MS,
            ),
            ("environment", "EVAL", environment::EVAL),
            ("environment", "EVAL_EXTERNAL", environment::EVAL_EXTERNAL),
            ("environment", "PLAYGROUND", environment::PLAYGROUND),
            (
                "environment",
                "PLAYGROUND_EXTERNAL",
                environment::PLAYGROUND_EXTERNAL,
            ),
            ("environment", "EXPERIMENT", environment::EXPERIMENT),
            ("media", "URI_SCHEME", media::URI_SCHEME),
            ("media", "DROPPED_URI", media::DROPPED_URI),
            (
                "marker",
                "TRUNCATED_STRING_SUFFIX_PREFIX",
                marker::TRUNCATED_STRING_SUFFIX_PREFIX,
            ),
            (
                "marker",
                "TRUNCATED_MESSAGES_PREFIX",
                marker::TRUNCATED_MESSAGES_PREFIX,
            ),
            ("marker", "MAX_DEPTH", marker::MAX_DEPTH),
        ];
        rows.iter()
            .map(|(module, name, value)| format!("{module}::{name} = \"{value}\""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn reserved_environments() {
        assert!(environment::is_reserved(environment::EVAL));
        assert!(environment::is_reserved("cetra-playground"));
        assert!(!environment::is_reserved("production"));
        assert!(!environment::is_reserved("cetra-eval-external"));
        assert!(!environment::is_reserved("cetra-playground-external"));
        assert!(!environment::is_reserved(""));

        assert_eq!(
            environment::external_alias(environment::EVAL),
            Some(environment::EVAL_EXTERNAL)
        );
        assert_eq!(
            environment::external_alias(environment::PLAYGROUND),
            Some(environment::PLAYGROUND_EXTERNAL)
        );
        assert_eq!(environment::external_alias("production"), None);
        assert_eq!(
            environment::external_alias(environment::EVAL_EXTERNAL),
            None
        );
        for reserved in environment::RESERVED {
            assert!(
                environment::is_reserved(reserved),
                "{reserved} is not reserved"
            );
            assert!(
                environment::external_alias(reserved).is_some(),
                "{reserved} has no external alias"
            );
        }
    }

    #[test]
    fn names_snapshot() {
        insta::with_settings!({
            snapshot_path => "../tests/snapshots",
            prepend_module_to_snapshot => false,
        }, {
            insta::assert_snapshot!("names", catalog());
        });
    }

    #[test]
    fn branded_constants_start_with_the_product_name() {
        let branded: &[&str] = &[
            attr::SPAN_KIND,
            attr::INPUT,
            attr::INPUT_FORMAT,
            attr::OUTPUT,
            attr::OUTPUT_FORMAT,
            attr::SESSION_ID,
            attr::USER_ID,
            attr::TAGS,
            attr::METADATA_PREFIX,
            attr::ENVIRONMENT,
            attr::PROMPT_NAME,
            attr::PROMPT_VERSION,
            attr::RELEASE,
            attr::TTFT_NS,
            attr::COST_TOTAL,
            attr::EXPERIMENT_ID,
            attr::DATASET_ITEM_ID,
            metadata_key::EXPERIMENT_ID,
            metadata_key::DATASET_ITEM_ID,
            metadata_key::EVAL_TARGET_TRACE_ID,
            metadata_key::EVAL_TARGET_SPAN_ID,
            metadata_key::EVAL_RULE_ID,
            metadata_key::EVAL_EXPERIMENT_ID,
            metadata_key::EVAL_EVALUATOR_ID,
            metadata_key::NORMALIZE_ERROR,
            metadata_key::NORMALIZE_WARNING,
            metadata_key::PLAYGROUND_RUN_BY,
            metadata_key::PLAYGROUND_SOURCE,
            environment::EVAL,
            environment::EVAL_EXTERNAL,
            environment::PLAYGROUND,
            environment::PLAYGROUND_EXTERNAL,
            media::DROPPED_URI,
        ];
        for value in branded {
            assert!(
                value.starts_with(PRODUCT),
                "{value} does not start with {PRODUCT}"
            );
        }
        assert!(marker::TRUNCATED_STRING_SUFFIX_PREFIX.contains(PRODUCT));
        assert!(marker::TRUNCATED_MESSAGES_PREFIX.contains(PRODUCT));
        assert!(marker::MAX_DEPTH.contains(PRODUCT));
    }

    /// A new attribute constant joins this list and the site's attribute page (D-W-7).
    const ATTRIBUTES: &[&str] = &[
        attr::SPAN_KIND,
        attr::INPUT,
        attr::INPUT_FORMAT,
        attr::OUTPUT,
        attr::OUTPUT_FORMAT,
        attr::SESSION_ID,
        attr::USER_ID,
        attr::TAGS,
        attr::METADATA_PREFIX,
        attr::ENVIRONMENT,
        attr::PROMPT_NAME,
        attr::PROMPT_VERSION,
        attr::RELEASE,
        attr::TTFT_NS,
        attr::COST_TOTAL,
        attr::EXPERIMENT_ID,
        attr::DATASET_ITEM_ID,
        metadata_key::EXPERIMENT_ID,
        metadata_key::DATASET_ITEM_ID,
        metadata_key::EVAL_TARGET_TRACE_ID,
        metadata_key::EVAL_TARGET_SPAN_ID,
        metadata_key::EVAL_RULE_ID,
        metadata_key::EVAL_EXPERIMENT_ID,
        metadata_key::EVAL_EVALUATOR_ID,
        metadata_key::NORMALIZE_ERROR,
        metadata_key::NORMALIZE_WARNING,
        metadata_key::PLAYGROUND_RUN_BY,
        metadata_key::PLAYGROUND_SOURCE,
    ];

    #[test]
    fn the_attribute_reference_names_every_cetra_attribute() {
        let page = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../website/src/content/docs/docs/reference/attributes.md"
        ));
        // Every `cetra.`-prefixed attribute written in backticks on the page.
        let documented: std::collections::BTreeSet<&str> = page
            .split('`')
            .skip(1)
            .step_by(2)
            .filter(|token| token.starts_with("cetra."))
            .collect();
        // A prefix constant is written with a `<key>` placeholder.
        let expected: std::collections::BTreeSet<String> = ATTRIBUTES
            .iter()
            .map(|name| {
                if name.ends_with('.') {
                    format!("{name}<key>")
                } else {
                    (*name).to_owned()
                }
            })
            .collect();
        let expected: std::collections::BTreeSet<&str> =
            expected.iter().map(String::as_str).collect();
        assert_eq!(documented, expected);
    }
}
