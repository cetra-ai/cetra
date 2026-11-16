//! Model price request and response types (spec 09 § Pricing & GDPR, spec 04 § Pricing, D-T-7).

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::common::Limit;

/// Who owns a price row. Project beats org, which beats built-in (spec 02 rule 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelPriceScope {
    /// The embedded LiteLLM snapshot. Read-only.
    Builtin,
    /// A row of the organization; applies to all its projects.
    Org,
    /// A row of one project.
    Project,
}

/// One long-context band. A null price uses the row's base price.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPriceTier {
    /// The band applies when the span's input tokens (cache included) are at least this count.
    pub min_input_tokens: i64,
    /// USD per 1M uncached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_input: Option<f64>,
    /// USD per 1M output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_output: Option<f64>,
    /// USD per 1M cache-read tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cache_read: Option<f64>,
    /// USD per 1M cache-write tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cache_write: Option<f64>,
    /// USD per 1M reasoning tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_reasoning: Option<f64>,
}

/// A price row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ModelPrice {
    /// Row id. Absent on built-in rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Who owns the row.
    pub scope: ModelPriceScope,
    /// Display name.
    pub model_name: String,
    /// Regular expression matched against the span's model, anchored and case-insensitive.
    pub match_pattern: String,
    /// When set, the row prices spans of this provider (or with none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The row prices spans that started at or after this time. JSON is RFC 3339.
    #[serde(with = "cetra_core::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub valid_from: i64,
    /// USD per 1M uncached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_input: Option<f64>,
    /// USD per 1M output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_output: Option<f64>,
    /// USD per 1M cache-read tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cache_read: Option<f64>,
    /// USD per 1M cache-write tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cache_write: Option<f64>,
    /// USD per 1M reasoning tokens. Absent: the output price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_reasoning: Option<f64>,
    /// Long-context bands, by increasing `min_input_tokens`. Empty when none.
    pub tiers: Vec<ModelPriceTier>,
    /// Creation time. Absent on built-in rows. JSON is RFC 3339.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub created_at: Option<i64>,
    /// Last change. Absent on built-in rows. JSON is RFC 3339.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::api::keys::optional_time"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub updated_at: Option<i64>,
}

/// Body of `POST /models` and `PATCH /models/{id}`. A PATCH replaces the whole row: an absent
/// optional field becomes null (D-T-7).
#[derive(Debug, Clone, PartialEq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPriceInput {
    /// Display name, 1–200 characters after trimming.
    pub model_name: String,
    /// Regular expression, 1–500 bytes. Absent or blank: exactly `model_name`.
    #[serde(default)]
    pub match_pattern: Option<String>,
    /// Provider, such as `openai` or `gcp.vertex_ai`. Absent or blank: any provider.
    #[serde(default)]
    pub provider: Option<String>,
    /// Absent: the row applies at any time. JSON is RFC 3339.
    #[serde(default, with = "crate::api::keys::optional_time")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub valid_from: Option<i64>,
    /// USD per 1M uncached input tokens, 0–1 000 000.
    #[serde(default)]
    pub price_input: Option<f64>,
    /// USD per 1M output tokens, 0–1 000 000.
    #[serde(default)]
    pub price_output: Option<f64>,
    /// USD per 1M cache-read tokens.
    #[serde(default)]
    pub price_cache_read: Option<f64>,
    /// USD per 1M cache-write tokens.
    #[serde(default)]
    pub price_cache_write: Option<f64>,
    /// USD per 1M reasoning tokens. Absent: the output price.
    #[serde(default)]
    pub price_reasoning: Option<f64>,
    /// Up to 8 bands, by strictly increasing `min_input_tokens`.
    #[serde(default)]
    pub tiers: Vec<ModelPriceTier>,
}

/// Query parameters of `GET /api/v1/projects/{project}/models`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ModelPriceListParams {
    /// Only rows of this scope.
    #[serde(default)]
    pub scope: Option<ModelPriceScope>,
    /// Case-insensitive substring of `model_name`.
    #[serde(default)]
    pub q: Option<String>,
    /// Maximum number of items to return (1–1000, default 50).
    #[serde(default)]
    #[param(minimum = 1, maximum = 1000, default = 50)]
    pub limit: Limit,
    /// Opaque cursor from a previous page.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// Query parameters of `GET /api/v1/projects/{project}/models/resolve`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ModelPriceResolveParams {
    /// The model name a span would carry.
    pub model: String,
    /// The provider a span would carry. Absent: none.
    #[serde(default)]
    pub provider: Option<String>,
}

/// The row that would price a span now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ModelPriceResolution {
    /// The winning row. Absent when no row prices the model (spec 09: optional fields are
    /// absent, never null).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<ModelPrice>,
}
