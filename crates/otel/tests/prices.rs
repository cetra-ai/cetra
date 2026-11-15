//! Built-in price snapshot.
//!
//! Conversion from LiteLLM's `model_prices_and_context_window.json`:
//! - Keep entries whose `mode` is `chat`, `completion`, `responses` or `embedding`,
//!   with at least one of `input_cost_per_token` / `output_cost_per_token`.
//! - The model name is the LiteLLM key with a leading `<litellm_provider>/` removed.
//!   A key that still contains `/` after that is skipped (proxy routes such as `openrouter/…`).
//! - One row per `(model_name, provider)`. When two kept entries give the same pair, keep the
//!   entry whose key has no `/`. Otherwise keep the first in key order.
//! - `match_pattern` is `regex::escape(model_name)`.
//! - `provider` is `row_provider(litellm_provider)`, the semconv name of the publisher or
//!   cloud platform (see `row_provider`). A LiteLLM provider with no mapping keeps its own
//!   name, so its rows never price another provider's spans. A missing `litellm_provider`
//!   gives null.
//! - Rows are sorted by `(model_name, provider_rank, provider)` (see `provider_rank`). A span
//!   with no provider takes the first matching row, so the publisher's row wins.
//! - Prices are per-token costs times 1e6: `input_cost_per_token` → `price_input`,
//!   `output_cost_per_token` → `price_output`, `cache_read_input_token_cost` →
//!   `price_cache_read`, `cache_creation_input_token_cost` → `price_cache_write`,
//!   `output_cost_per_reasoning_token` → `price_reasoning`.
//! - Tiers: `*_above_<N>k_tokens` → `min_input_tokens = N·1000`, with the matching prices.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::PathBuf;

use llmobs_otel::{PriceRow, PriceScope, PriceTable, PriceTier, builtin_price_rows};
use serde_json::{Map, Value};

const KEEP_MODES: &[&str] = &["chat", "completion", "responses", "embedding"];

const REQUIRED_MODELS: &[&str] = &[
    "gpt-4o",
    "gpt-4o-mini",
    "claude-sonnet-4-5",
    "gemini-2.5-pro",
    "text-embedding-3-small",
];

#[test]
fn builtin_snapshot_is_valid() {
    let text = fs::read_to_string(snapshot_path()).expect("snapshot reads");
    let rows = builtin_price_rows();
    let canonical = canonical_json(&rows);
    assert!(
        text == canonical,
        "snapshot is not canonical JSON at {}",
        first_diff(&text, &canonical)
    );
    assert!(
        PriceTable::new(rows.clone()).is_ok(),
        "snapshot patterns compile"
    );

    let mut seen = HashSet::new();
    for row in &rows {
        assert_eq!(row.scope, PriceScope::Builtin);
        assert_eq!(row.valid_from, 0);
        assert!(
            !row.match_pattern.contains('^') && !row.match_pattern.contains('$'),
            "pattern contains an anchor"
        );
        check_prices(row);
        assert!(
            seen.insert((
                row.match_pattern.clone(),
                row.provider.clone(),
                row.valid_from,
            )),
            "duplicate pattern"
        );
    }

    for model in REQUIRED_MODELS {
        assert!(
            rows.iter()
                .any(|row| pattern_matches(&row.match_pattern, model)),
            "snapshot does not match a required model"
        );
    }
}

#[test]
fn conversion_follows_the_litellm_rules() {
    let raw = serde_json::json!({
        "openrouter/openai/gpt-4o": {
            "mode": "chat",
            "litellm_provider": "openrouter",
            "input_cost_per_token": 9e-6
        },
        "azure/gpt-4o": {
            "mode": "chat",
            "litellm_provider": "azure",
            "input_cost_per_token": 1e-6
        },
        "gpt-4o": {
            "mode": "chat",
            "litellm_provider": "openai",
            "input_cost_per_token": 2.5e-6,
            "output_cost_per_token": 1e-5,
            "cache_read_input_token_cost": 1.25e-6,
            "cache_creation_input_token_cost": 3.75e-6,
            "output_cost_per_reasoning_token": 5e-6,
            "output_cost_per_token_above_128k_tokens": 4e-6,
            "input_cost_per_token_above_200k_tokens": 5e-6,
            "cache_creation_input_token_cost_above_1hr": 8e-6,
            "input_cost_per_token_above_200k_tokens_priority": 9e-6
        },
        "openai/gpt-4o": {
            "mode": "chat",
            "litellm_provider": "openai",
            "input_cost_per_token": 3e-6
        },
        "picture": {
            "mode": "image_generation",
            "litellm_provider": "openai",
            "input_cost_per_token": 1e-6
        },
        "no-cost": { "mode": "chat", "litellm_provider": "openai" },
        "cache-only": {
            "mode": "chat",
            "litellm_provider": "openai",
            "cache_read_input_token_cost": 1e-6
        },
        "no-provider": { "mode": "chat", "input_cost_per_token": 1e-6 },
        "a.b": { "mode": "completion", "litellm_provider": "groq", "input_cost_per_token": 1e-6 },
        "embed-me": {
            "mode": "embedding",
            "litellm_provider": "openai",
            "input_cost_per_token": 2e-8,
            "output_cost_per_token": 0.0
        },
        "reply": { "mode": "responses", "litellm_provider": "gemini", "input_cost_per_token": 1e-6 },
        "gemini-x": { "mode": "chat", "litellm_provider": "gemini", "input_cost_per_token": 5e-6 },
        "gemini/gemini-x": {
            "mode": "chat",
            "litellm_provider": "gemini",
            "input_cost_per_token": 4e-6
        },
        "vertex": {
            "mode": "chat",
            "litellm_provider": "vertex_ai-language-models",
            "input_cost_per_token": 1e-6
        },
        "bedrock-model": {
            "mode": "chat",
            "litellm_provider": "bedrock_converse",
            "input_cost_per_token": 1e-6
        },
        "azure-model": {
            "mode": "chat",
            "litellm_provider": "azure_ai",
            "input_cost_per_token": 1e-6
        },
        "mistral-model": {
            "mode": "chat",
            "litellm_provider": "mistral",
            "input_cost_per_token": 1e-6
        },
        "x-model": { "mode": "chat", "litellm_provider": "xai", "input_cost_per_token": 1e-6 },
        "deep": { "mode": "chat", "litellm_provider": "deepseek", "input_cost_per_token": 1e-6 },
        "cohere-model": {
            "mode": "chat",
            "litellm_provider": "cohere",
            "input_cost_per_token": 1e-6
        },
        "other": {
            "mode": "chat",
            "litellm_provider": "fireworks_ai",
            "input_cost_per_token": 1e-6
        },
        "azure/shared": {
            "mode": "chat",
            "litellm_provider": "azure",
            "input_cost_per_token": 1e-6
        },
        "azure_ai/shared": {
            "mode": "chat",
            "litellm_provider": "azure_ai",
            "input_cost_per_token": 4e-6
        },
        "openai/shared": {
            "mode": "chat",
            "litellm_provider": "openai",
            "input_cost_per_token": 9e-6
        }
    });
    let entries = raw.as_object().expect("object");
    let (rows, collisions) = convert(entries);
    // `openai/gpt-4o` and `gpt-4o`, and `gemini/gemini-x` and `gemini-x`, share a
    // (model, provider) pair. The other keys differ in provider.
    assert_eq!(collisions, 2);
    assert_eq!(
        rows.iter()
            .map(|row| (row.model_name.as_str(), row.provider.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            ("a.b", Some("groq")),
            ("azure-model", Some("azure.ai.inference")),
            ("bedrock-model", Some("aws.bedrock")),
            ("cohere-model", Some("cohere")),
            ("deep", Some("deepseek")),
            ("embed-me", Some("openai")),
            ("gemini-x", Some("gcp.gemini")),
            ("gpt-4o", Some("openai")),
            ("gpt-4o", Some("azure.ai.openai")),
            ("mistral-model", Some("mistral_ai")),
            ("no-provider", None),
            ("other", Some("fireworks_ai")),
            ("reply", Some("gcp.gemini")),
            ("shared", Some("openai")),
            ("shared", Some("azure.ai.inference")),
            ("shared", Some("azure.ai.openai")),
            ("vertex", Some("gcp.vertex_ai")),
            ("x-model", Some("x_ai")),
        ]
    );

    let gpt = find(&rows, "gpt-4o", Some("openai"));
    assert_eq!(gpt.scope, PriceScope::Builtin);
    assert_eq!(gpt.valid_from, 0);
    assert_eq!(gpt.match_pattern, regex::escape("gpt-4o"));
    assert_eq!(gpt.price_input, Some(million(2.5e-6)));
    assert_eq!(gpt.price_output, Some(million(1e-5)));
    assert_eq!(gpt.price_cache_read, Some(million(1.25e-6)));
    assert_eq!(gpt.price_cache_write, Some(million(3.75e-6)));
    assert_eq!(gpt.price_reasoning, Some(million(5e-6)));
    assert_eq!(
        gpt.tiers,
        Some(vec![
            PriceTier {
                min_input_tokens: 128_000,
                price_input: None,
                price_output: Some(million(4e-6)),
                price_cache_read: None,
                price_cache_write: None,
                price_reasoning: None,
            },
            PriceTier {
                min_input_tokens: 200_000,
                price_input: Some(million(5e-6)),
                price_output: None,
                price_cache_read: None,
                price_cache_write: None,
                price_reasoning: None,
            },
        ])
    );
    assert_eq!(
        find(&rows, "gpt-4o", Some("azure.ai.openai")).price_input,
        Some(million(1e-6))
    );
    assert_eq!(
        find(&rows, "gemini-x", Some("gcp.gemini")).price_input,
        Some(million(5e-6))
    );

    let dotted = find(&rows, "a.b", Some("groq"));
    assert_eq!(dotted.match_pattern, regex::escape("a.b"));
    assert_eq!(
        find(&rows, "embed-me", Some("openai")).price_output,
        Some(0.0)
    );
    assert_eq!(
        find(&rows, "other", Some("fireworks_ai")).price_input,
        Some(million(1e-6))
    );
    assert_eq!(
        find(&rows, "no-provider", None).price_input,
        Some(million(1e-6))
    );
    assert_eq!(
        find(&rows, "shared", Some("openai")).price_input,
        Some(million(9e-6))
    );
    assert_eq!(
        find(&rows, "shared", Some("azure.ai.inference")).price_input,
        Some(million(4e-6))
    );
    assert_eq!(
        find(&rows, "shared", Some("azure.ai.openai")).price_input,
        Some(million(1e-6))
    );
}

/// `row_provider` maps each LiteLLM provider name to its semconv name. A name with no
/// mapping keeps its own name, so its rows never price another provider's spans.
#[test]
fn provider_mapping_covers_the_litellm_names() {
    let cases = [
        ("openai", "openai"),
        ("text-completion-openai", "openai"),
        ("anthropic", "anthropic"),
        ("gemini", "gcp.gemini"),
        ("vertex_ai-anthropic_models", "gcp.vertex_ai"),
        ("vertex_ai", "gcp.vertex_ai"),
        ("bedrock", "aws.bedrock"),
        ("bedrock_converse", "aws.bedrock"),
        ("azure", "azure.ai.openai"),
        ("azure_text", "azure.ai.openai"),
        ("azure_ai", "azure.ai.inference"),
        ("mistral", "mistral_ai"),
        ("codestral", "mistral_ai"),
        ("text-completion-codestral", "mistral_ai"),
        ("xai", "x_ai"),
        ("groq", "groq"),
        ("deepseek", "deepseek"),
        ("cohere", "cohere"),
        ("cohere_chat", "cohere"),
        ("perplexity", "perplexity"),
        ("watsonx", "ibm.watsonx.ai"),
        ("fireworks_ai", "fireworks_ai"),
        ("azure_anthropic", "azure_anthropic"),
    ];
    for (litellm, expected) in cases {
        assert_eq!(row_provider(litellm), expected, "{litellm}");
    }
}

#[test]
fn rows_put_the_publisher_first() {
    let raw = serde_json::json!({
        "vertex_ai/gemini-2.5-flash": {
            "mode": "chat", "litellm_provider": "vertex_ai", "input_cost_per_token": 3e-7
        },
        "gemini/gemini-2.5-flash": {
            "mode": "chat", "litellm_provider": "gemini", "input_cost_per_token": 3e-7
        },
        "bedrock/gemini-2.5-flash": {
            "mode": "chat", "litellm_provider": "bedrock", "input_cost_per_token": 3e-7
        },
        "fireworks_ai/gemini-2.5-flash": {
            "mode": "chat", "litellm_provider": "fireworks_ai", "input_cost_per_token": 3e-7
        },
        "gemini-2.5-flash": {
            "mode": "chat", "input_cost_per_token": 3e-7
        }
    });
    let (rows, collisions) = convert(raw.as_object().expect("object"));
    // Four providers, so no collision. Cloud platforms sort by provider name.
    assert_eq!(collisions, 0);
    assert_eq!(
        rows.iter()
            .map(|row| row.provider.as_deref())
            .collect::<Vec<_>>(),
        vec![
            Some("gcp.gemini"),
            Some("aws.bedrock"),
            Some("gcp.vertex_ai"),
            Some("fireworks_ai"),
            None,
        ]
    );
}

/// Rewrites `prices/builtin.json` when `LLMOBS_LITELLM_JSON` points at a download.
#[test]
#[allow(clippy::print_stderr)]
fn refresh() {
    let Ok(path) = std::env::var("LLMOBS_LITELLM_JSON") else {
        return;
    };
    let text = fs::read_to_string(&path).expect("LiteLLM JSON reads");
    let value: Value = serde_json::from_str(&text).expect("LiteLLM JSON parses");
    let entries = value.as_object().expect("LiteLLM JSON is an object");
    let (rows, collisions) = convert(entries);
    // `per_token * 1e6` can serialize as a long decimal that parses back to a
    // shorter one. The committed file is that shorter form.
    let text = canonical_json(&rows);
    let rows: Vec<PriceRow> = serde_json::from_str(&text).expect("rows round-trip");
    let text = canonical_json(&rows);
    fs::write(snapshot_path(), &text).expect("snapshot writes");
    eprintln!("litellm price rows: {}", rows.len());
    eprintln!("litellm price collisions: {collisions}");
    eprintln!("litellm price bytes: {}", text.len());
}

fn convert(entries: &Map<String, Value>) -> (Vec<PriceRow>, usize) {
    let mut winners: BTreeMap<(String, Option<String>), Winner> = BTreeMap::new();
    let mut collisions = 0;
    for (key, value) in entries {
        let Some(entry) = value.as_object() else {
            continue;
        };
        let Some(row) = convert_entry(key, entry) else {
            continue;
        };
        let bare_key = !key.contains('/');
        let id = (row.model_name.clone(), row.provider.clone());
        if let Some(current) = winners.get_mut(&id) {
            collisions += 1;
            if !current.bare_key && bare_key {
                *current = Winner {
                    bare_key: true,
                    row,
                };
            }
        } else {
            winners.insert(id, Winner { bare_key, row });
        }
    }
    let mut rows: Vec<PriceRow> = winners.into_values().map(|winner| winner.row).collect();
    rows.sort_by(|left, right| {
        (
            &left.model_name,
            provider_rank(left.provider.as_deref()),
            &left.provider,
        )
            .cmp(&(
                &right.model_name,
                provider_rank(right.provider.as_deref()),
                &right.provider,
            ))
    });
    (rows, collisions)
}

struct Winner {
    bare_key: bool,
    row: PriceRow,
}

fn convert_entry(key: &str, entry: &Map<String, Value>) -> Option<PriceRow> {
    let mode = entry.get("mode")?.as_str()?;
    if !KEEP_MODES.contains(&mode) {
        return None;
    }
    let litellm_provider = entry.get("litellm_provider").and_then(Value::as_str);
    let model_name = model_name(key, litellm_provider)?;
    let match_pattern = regex::escape(&model_name);
    let price_input = field_price(entry, "input_cost_per_token");
    let price_output = field_price(entry, "output_cost_per_token");
    if price_input.is_none() && price_output.is_none() {
        return None;
    }
    Some(PriceRow {
        scope: PriceScope::Builtin,
        model_name,
        match_pattern,
        provider: litellm_provider.map(row_provider),
        valid_from: 0,
        price_input,
        price_output,
        price_cache_read: field_price(entry, "cache_read_input_token_cost"),
        price_cache_write: field_price(entry, "cache_creation_input_token_cost"),
        price_reasoning: field_price(entry, "output_cost_per_reasoning_token"),
        tiers: tier_list(entry),
    })
}

fn model_name(key: &str, provider: Option<&str>) -> Option<String> {
    let stripped = match provider {
        Some(provider) => {
            let prefix = format!("{provider}/");
            key.strip_prefix(&prefix).unwrap_or(key)
        }
        None => key,
    };
    if stripped.is_empty() || stripped.contains('/') {
        None
    } else {
        Some(stripped.to_owned())
    }
}

/// The row provider for a LiteLLM `litellm_provider`. A name with no mapping keeps its
/// LiteLLM name, lowercased. Exact names come before the prefix arms, so `azure_ai`
/// is not an `azure` provider.
fn row_provider(litellm_provider: &str) -> String {
    let lower = litellm_provider.to_ascii_lowercase();
    let mapped: &str = match lower.as_str() {
        "openai" | "text-completion-openai" => "openai",
        "anthropic" => "anthropic",
        "gemini" => "gcp.gemini",
        "azure" | "azure_text" => "azure.ai.openai",
        "azure_ai" => "azure.ai.inference",
        "mistral" | "codestral" | "text-completion-codestral" => "mistral_ai",
        "xai" => "x_ai",
        "groq" => "groq",
        "deepseek" => "deepseek",
        "cohere" | "cohere_chat" => "cohere",
        "perplexity" => "perplexity",
        "watsonx" => "ibm.watsonx.ai",
        other if other.starts_with("vertex_ai") => "gcp.vertex_ai",
        other if other.starts_with("bedrock") => "aws.bedrock",
        other => other,
    };
    mapped.to_owned()
}

/// 0 the model publisher's own API, 1 a cloud platform, 2 any other named provider,
/// 3 no provider.
fn provider_rank(provider: Option<&str>) -> u8 {
    match provider {
        None => 3,
        Some(
            "openai" | "anthropic" | "gcp.gemini" | "x_ai" | "mistral_ai" | "deepseek" | "cohere",
        ) => 0,
        Some(
            "gcp.vertex_ai" | "aws.bedrock" | "azure.ai.openai" | "azure.ai.inference"
            | "ibm.watsonx.ai",
        ) => 1,
        Some(_) => 2,
    }
}

fn field_price(entry: &Map<String, Value>, field: &str) -> Option<f64> {
    entry.get(field).and_then(per_million)
}

fn per_million(value: &Value) -> Option<f64> {
    let number = value.as_f64()?;
    if !number.is_finite() {
        return None;
    }
    // Per-token × 1e6 carries binary noise (3.96e-6 × 1e6 = 3.9600000000000004). LiteLLM
    // prices have at most 15 significant digits, so round to those.
    let price: f64 = format!("{:.14e}", number * 1_000_000.0).parse().ok()?;
    price.is_finite().then_some(price)
}

#[derive(Clone, Copy)]
enum TierPrice {
    Input,
    Output,
    CacheRead,
    CacheWrite,
    Reasoning,
}

fn tier_list(entry: &Map<String, Value>) -> Option<Vec<PriceTier>> {
    let mut bands: BTreeMap<i64, PriceTier> = BTreeMap::new();
    for (field, value) in entry {
        let Some((kind, min_input_tokens)) = tier_field(field) else {
            continue;
        };
        let Some(price) = per_million(value) else {
            continue;
        };
        let tier = bands.entry(min_input_tokens).or_insert_with(|| PriceTier {
            min_input_tokens,
            price_input: None,
            price_output: None,
            price_cache_read: None,
            price_cache_write: None,
            price_reasoning: None,
        });
        match kind {
            TierPrice::Input => tier.price_input = Some(price),
            TierPrice::Output => tier.price_output = Some(price),
            TierPrice::CacheRead => tier.price_cache_read = Some(price),
            TierPrice::CacheWrite => tier.price_cache_write = Some(price),
            TierPrice::Reasoning => tier.price_reasoning = Some(price),
        }
    }
    if bands.is_empty() {
        None
    } else {
        Some(bands.into_values().collect())
    }
}

fn tier_field(field: &str) -> Option<(TierPrice, i64)> {
    const PREFIXES: &[(&str, TierPrice)] = &[
        ("input_cost_per_token_above_", TierPrice::Input),
        ("output_cost_per_token_above_", TierPrice::Output),
        ("cache_read_input_token_cost_above_", TierPrice::CacheRead),
        (
            "cache_creation_input_token_cost_above_",
            TierPrice::CacheWrite,
        ),
        (
            "output_cost_per_reasoning_token_above_",
            TierPrice::Reasoning,
        ),
    ];
    for (prefix, kind) in PREFIXES {
        let Some(rest) = field.strip_prefix(prefix) else {
            continue;
        };
        let digits = rest.strip_suffix("k_tokens")?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let count: i64 = digits.parse().ok()?;
        let min_input_tokens = count.checked_mul(1_000)?;
        return Some((*kind, min_input_tokens));
    }
    None
}

fn pattern_matches(pattern: &str, model: &str) -> bool {
    let anchored = format!("^(?i:{pattern})$");
    regex::Regex::new(&anchored)
        .expect("pattern")
        .is_match(model)
}

fn check_prices(row: &PriceRow) {
    for price in [
        row.price_input,
        row.price_output,
        row.price_cache_read,
        row.price_cache_write,
        row.price_reasoning,
    ] {
        check_price(price);
    }
    if let Some(tiers) = &row.tiers {
        for tier in tiers {
            assert!(tier.min_input_tokens >= 0);
            for price in [
                tier.price_input,
                tier.price_output,
                tier.price_cache_read,
                tier.price_cache_write,
                tier.price_reasoning,
            ] {
                check_price(price);
            }
        }
    }
}

fn check_price(price: Option<f64>) {
    if let Some(price) = price {
        assert!(price.is_finite() && price >= 0.0);
    }
}

fn find<'a>(rows: &'a [PriceRow], model: &str, provider: Option<&str>) -> &'a PriceRow {
    rows.iter()
        .find(|row| row.model_name == model && row.provider.as_deref() == provider)
        .expect("row")
}

fn million(per_token: f64) -> f64 {
    per_token * 1_000_000.0
}

fn first_diff(left: &str, right: &str) -> String {
    let mut bytes = left.bytes().zip(right.bytes());
    let index = bytes
        .position(|(left, right)| left != right)
        .unwrap_or(left.len().min(right.len()));
    let start = left.floor_char_boundary(index.saturating_sub(40));
    let end = left.ceil_char_boundary((index + 40).min(left.len()));
    let right_end = right.ceil_char_boundary(end.min(right.len()));
    format!(
        "{index} file {:?} canonical {:?} lens {} {}",
        &left[start..end],
        &right[start.min(right.len())..right_end],
        left.len(),
        right.len()
    )
}

fn canonical_json(rows: &[PriceRow]) -> String {
    let value = serde_json::to_value(rows).expect("rows are JSON");
    let mut text = serde_json::to_string_pretty(&value).expect("value is JSON");
    text.push('\n');
    text
}

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("prices/builtin.json")
}
