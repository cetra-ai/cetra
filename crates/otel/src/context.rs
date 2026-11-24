//! Model, provider, operation, request params, and context fields.
//!
//! Spec 02 § Model, provider, usage, § Context fields, and rules 3, 8, 11 and 12.
//! The precedence tables live here, next to the functions that read them.
//! `service_name` is filled while the resource is encoded. A missing provider
//! is filled from the matched price row.

use std::collections::BTreeMap;

use cetra_core::enums::{Convention, SpanKind};
use cetra_core::names::{attr, metadata_key};
use serde_json::Value as Json;

use crate::bag::{AttrBag, Use};
use crate::fields::{ResourceView, Source, first_string, resource, span};
use crate::value::{as_i64, as_str, as_string_list, to_json, to_map_string};
use crate::warn::{Warning, Warnings};

const REQUEST_MODEL: &[Source] = &[
    span("gen_ai.request.model", Convention::Genai),
    span("llm.model_name", Convention::Openinference),
    span("embedding.model_name", Convention::Openinference),
    span("ai.model.id", Convention::Vercel),
    span("langfuse.observation.model.name", Convention::Langfuse),
];

const RESPONSE_MODEL: &[Source] = &[
    span("gen_ai.response.model", Convention::Genai),
    span("ai.response.model", Convention::Vercel),
    span("llm.response.model_name", Convention::Openinference),
];

const PROVIDER: &[Source] = &[
    span("gen_ai.provider.name", Convention::Genai),
    span("gen_ai.system", Convention::Genai),
    span("llm.provider", Convention::Openinference),
    span("llm.system", Convention::Openinference),
    span("ai.model.provider", Convention::Vercel),
];

/// Vercel v7 `context` call option, recorded under `ai.settings.`.
const VERCEL_CONTEXT: &str = "ai.settings.context.";

/// Values of `gen_ai.provider.name` and `gen_ai.system` that name a framework,
/// never a provider (D-B1-8, D-IN-15, D-IN-23, D-IN-30). Compared after
/// lowercasing. Sorted (test `framework_names_are_sorted`).
const FRAMEWORK_NAMES: &[&str] = &[
    "autogen",
    "browser_use",
    "gcp.vertex.agent",
    "koog",
    "microsoft.agent_framework",
    "spring_ai",
    "strands-agents",
];

/// Keys that carry the endpoint, in order (D-IN-2). Read with `get`, never consumed.
const HOST_KEYS: &[&str] = &["server.address", "url.full", "gen_ai.openai.api_base"];

/// Endpoint host → provider (D-IN-2). An entry starting with `*.` matches a host
/// that ends with the rest preceded by a dot. The first match wins.
const ENDPOINT_PROVIDERS: &[(&str, &str)] = &[
    ("openrouter.ai", "openrouter"),
    ("api.groq.com", "groq"),
    ("api.x.ai", "x_ai"),
    ("api.deepseek.com", "deepseek"),
    ("api.fireworks.ai", "fireworks_ai"),
    ("api.together.xyz", "together_ai"),
    ("api-inference.together.ai", "together_ai"),
    ("api.together.ai", "together_ai"),
    ("api.novita.ai", "novita"),
    ("dashscope.aliyuncs.com", "dashscope"),
    ("dashscope-intl.aliyuncs.com", "dashscope"),
    ("*.maas.aliyuncs.com", "dashscope"),
    ("router.huggingface.co", "huggingface"),
    ("api.cloudflare.com", "cloudflare"),
    ("*.cloud.databricks.com", "databricks"),
    ("*.azuredatabricks.net", "databricks"),
    ("*.gcp.databricks.com", "databricks"),
    ("api.cerebras.ai", "cerebras"),
    ("api.perplexity.ai", "perplexity"),
    ("api.moonshot.ai", "moonshot"),
    ("api.moonshot.cn", "moonshot"),
    ("api.z.ai", "zai"),
    ("open.bigmodel.cn", "zai"),
    ("api.sambanova.ai", "sambanova"),
    ("api.studio.nebius.com", "nebius"),
    ("api.tokenfactory.nebius.com", "nebius"),
    ("api.mistral.ai", "mistral_ai"),
    ("*.openai.azure.com", "azure.ai.openai"),
    // Azure AI Foundry serves several publishers' models; their prices are azure.ai.inference rows.
    ("*.services.ai.azure.com", "azure.ai.inference"),
    ("*.cognitiveservices.azure.com", "azure.ai.openai"),
    ("api.deepinfra.com", "deepinfra"),
    ("inference.baseten.co", "baseten"),
    ("*.api.baseten.co", "baseten"),
    ("api.hyperbolic.xyz", "hyperbolic"),
    ("integrate.api.nvidia.com", "nvidia_nim"),
    ("api.minimax.io", "minimax"),
    ("api.minimaxi.com", "minimax"),
    ("api.cohere.com", "cohere"),
    ("api.cohere.ai", "cohere"),
    ("ollama.com", "ollama"),
    ("aiplatform.googleapis.com", "gcp.vertex_ai"),
    ("ark.cn-beijing.volces.com", "volcengine"),
    ("ark.ap-southeast.bytepluses.com", "byteplus"),
    ("api.sarvam.ai", "sarvam"),
    ("qianfan.baidubce.com", "qianfan"),
    ("api.cometapi.com", "cometapi"),
    ("api.doubleword.ai", "doubleword"),
    ("ai-gateway.vercel.sh", "vercel_ai_gateway"),
    ("oai.helicone.ai", "helicone"),
    ("ai-gateway.helicone.ai", "helicone"),
    ("api.portkey.ai", "portkey"),
    ("gateway.ai.cloudflare.com", "cloudflare_ai_gateway"),
];

const OPERATION: &[Source] = &[span("gen_ai.operation.name", Convention::Genai)];

/// (name key, version key), in precedence order (D-V-16). Prompt keys are span
/// attributes only.
const PROMPT_SOURCES: &[(&str, &str)] = &[
    (attr::PROMPT_NAME, attr::PROMPT_VERSION),
    (
        "langfuse.observation.prompt.name",
        "langfuse.observation.prompt.version",
    ),
];

const SESSION_ID: &[Source] = &[
    span(attr::SESSION_ID, Convention::Cetra),
    span("session.id", Convention::Openinference),
    span("gen_ai.conversation.id", Convention::Genai),
    span(
        "traceloop.association.properties.session_id",
        Convention::Openllmetry,
    ),
    span("ai.telemetry.metadata.sessionId", Convention::Vercel),
    span("ai.settings.context.sessionId", Convention::Vercel),
    span("langfuse.session.id", Convention::Langfuse),
    span(
        "lmnr.association.properties.session_id",
        Convention::Laminar,
    ),
    span("gcp.vertex.agent.session_id", Convention::GoogleAdk),
    span("mirascope.ops.session.id", Convention::Genai),
];

const USER_ID: &[Source] = &[
    span(attr::USER_ID, Convention::Cetra),
    span("user.id", Convention::Openinference),
    span("enduser.id", Convention::Openinference),
    span(
        "traceloop.association.properties.user_id",
        Convention::Openllmetry,
    ),
    span("ai.telemetry.metadata.userId", Convention::Vercel),
    span("ai.settings.context.userId", Convention::Vercel),
    span("langfuse.user.id", Convention::Langfuse),
    span("lmnr.association.properties.user_id", Convention::Laminar),
    span("enduser.pseudo.id", Convention::Genai),
];

const TAGS: &[Source] = &[
    span(attr::TAGS, Convention::Cetra),
    span("tag.tags", Convention::Openinference),
    span("ai.telemetry.metadata.tags", Convention::Vercel),
    span("langfuse.trace.tags", Convention::Langfuse),
    span("lmnr.association.properties.tags", Convention::Laminar),
];

const ENVIRONMENT: &[Source] = &[
    span(attr::ENVIRONMENT, Convention::Cetra),
    resource("deployment.environment.name", Convention::Openinference),
    resource("deployment.environment", Convention::Openinference),
    span("langfuse.environment", Convention::Langfuse),
];

const RELEASE: &[Source] = &[
    span(attr::RELEASE, Convention::Cetra),
    resource("service.version", Convention::Openinference),
    span("langfuse.release", Convention::Langfuse),
    span("lmnr.span.release", Convention::Laminar),
];

/// Sorted so [`canonical_provider`] can binary-search it.
const WELL_KNOWN_PROVIDERS: &[&str] = &[
    "anthropic",
    "aws.bedrock",
    "azure.ai.inference",
    "azure.ai.openai",
    "cohere",
    "deepseek",
    "gcp.gemini",
    "gcp.gen_ai",
    "gcp.vertex_ai",
    "groq",
    "ibm.watsonx.ai",
    "mistral_ai",
    "openai",
    "perplexity",
    "x_ai",
];

/// Context keys that share a metadata prefix. They are not copied into `metadata`.
fn is_metadata_context_key(key: &str) -> bool {
    matches!(
        key,
        "ai.telemetry.metadata.sessionId"
            | "ai.telemetry.metadata.userId"
            | "ai.telemetry.metadata.tags"
            | "ai.settings.context.sessionId"
            | "ai.settings.context.userId"
            | "traceloop.association.properties.session_id"
            | "traceloop.association.properties.user_id"
    )
}

/// Scalar columns and context maps for one span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContextFields {
    /// Model named on the request.
    pub request_model: Option<String>,
    /// Model named on the response.
    pub response_model: Option<String>,
    /// Provider after rule 12, or the matched price row when the span named none.
    pub provider: Option<String>,
    /// `gen_ai.operation.name`, when it is a non-empty string.
    pub operation: Option<String>,
    /// Compact JSON object, keys sorted. `None` when there are no parameters.
    pub request_params: Option<String>,
    /// Session id.
    pub session_id: Option<String>,
    /// End-user id.
    pub user_id: Option<String>,
    /// Deployment environment.
    pub environment: Option<String>,
    /// Release or version.
    pub release: Option<String>,
    /// Registry prompt name (D-V-16). Stored as read, never checked against the registry.
    pub prompt_name: Option<String>,
    /// Registry prompt version, an integer ≥ 1, from the convention that named the prompt.
    pub prompt_version: Option<i64>,
    /// Tags, first-occurrence order, without duplicates.
    pub tags: Vec<String>,
    /// User metadata. Values follow rule 1.
    pub metadata: BTreeMap<String, String>,
}

/// Fill the scalar and context columns. Rule 3: these columns are filled for every kind.
///
/// `scope_name` is the span's instrumentation scope name (`""` when absent);
/// the Mirascope model-prefix rule is gated on it. `conventions` is the
/// conventions detected on the span; VoltAgent's model and session fills
/// (D-IN-6) are gated on it.
pub(crate) fn fill(
    bag: &mut AttrBag<'_>,
    resource: &ResourceView<'_>,
    scope_name: &str,
    conventions: &[Convention],
    warnings: &mut Warnings,
) -> ContextFields {
    let request_model = string_field(bag, resource, REQUEST_MODEL);
    let response_model = string_field(bag, resource, RESPONSE_MODEL);
    // Stripped before `provider` consumes `gen_ai.provider.name`.
    let request_model = mirascope_model(bag, scope_name, request_model);
    let response_model = mirascope_model(bag, scope_name, response_model);
    let request_model = voltagent_model(bag, conventions, request_model);
    let (request_model, response_model) =
        promptflow_models(bag, scope_name, request_model, response_model, warnings);
    let provider = provider(bag, resource, scope_name);
    let operation = string_field(bag, resource, OPERATION);
    let request_params = request_params(bag, warnings);
    let session_id = string_field(bag, resource, SESSION_ID);
    let session_id = voltagent_session(bag, conventions, session_id);
    let session_id = pipecat_session(bag, scope_name, session_id);
    let user_id = string_field(bag, resource, USER_ID);
    let environment = string_field(bag, resource, ENVIRONMENT);
    let release = string_field(bag, resource, RELEASE);
    let (prompt_name, prompt_version) = prompt(bag, warnings);
    let tags = tags(bag);
    let metadata = metadata(bag, warnings);
    ContextFields {
        request_model,
        response_model,
        provider,
        operation,
        request_params,
        session_id,
        user_id,
        environment,
        release,
        prompt_name,
        prompt_version,
        tags,
        metadata,
    }
}

/// D-V-16 and the pairing rule of spec 02 § Context fields. The first name
/// that is not blank wins. Its version is read from the same convention only;
/// the other convention's keys are never read. Without a winning name, a
/// version key is left in `attributes` with one `prompt_version_without_name`.
fn prompt(bag: &mut AttrBag<'_>, warnings: &mut Warnings) -> (Option<String>, Option<i64>) {
    let winner = PROMPT_SOURCES.iter().find_map(|&(name_key, version_key)| {
        let name = bag
            .get(name_key)
            .and_then(as_str)
            .filter(|text| !text.trim().is_empty())?;
        Some((name_key, name.to_owned(), version_key))
    });
    let Some((name_key, name, version_key)) = winner else {
        if PROMPT_SOURCES
            .iter()
            .any(|&(_, version_key)| bag.get(version_key).is_some())
        {
            warnings.add(Warning::PromptVersionWithoutName);
        }
        return (None, None);
    };
    bag.take(name_key);
    let version = match bag.get(version_key) {
        None => None,
        Some(value) => match as_i64(value) {
            Ok(Some(number)) if number >= 1 => {
                bag.take(version_key);
                Some(number)
            }
            _ => {
                warnings.add(Warning::InvalidPromptVersion);
                None
            }
        },
    };
    (Some(name), version)
}

/// Rule 12 over [`PROVIDER`]. A `gen_ai.provider.name` or `gen_ai.system` value
/// that names a framework ([`FRAMEWORK_NAMES`], D-IN-15, D-B1-8, D-IN-23) is
/// skipped and left unread, so the next source or the price table decides.
/// On scope `livekit-agents` a host-shaped provider decides first ([`livekit_provider`]).
fn provider(
    bag: &mut AttrBag<'_>,
    resource: &ResourceView<'_>,
    scope_name: &str,
) -> Option<String> {
    if let Some(provider) = livekit_provider(bag, scope_name) {
        return provider;
    }
    let sources: Vec<Source> = PROVIDER
        .iter()
        .copied()
        .filter(|source| !names_framework(bag, source.key))
        .collect();
    first_string(bag, resource, &sources).map(|resolved| {
        canonical_provider(&resolved.value, resolved.source.key == "ai.model.provider")
    })
}

/// R-LK-1 (D-IN-5, D-IN-2): on scope `livekit-agents`, a `gen_ai.provider.name`
/// is a host (LiveKit writes the netloc of its base URL) when it contains `:`,
/// parses as an IP address, or the endpoint table maps it (`api.openai.com`
/// is mapped to `openai`). The key is consumed. A mapped host gives its
/// provider; an unmapped host or IP gives no provider, and no later source is
/// read. Any other value (`azure.ai.openai`, `gcp.vertex_ai`) is not a host:
/// `None`, the key is left alone and the normal provider rules read it. No row
/// is added to [`ENDPOINT_PROVIDERS`].
fn livekit_provider(bag: &mut AttrBag<'_>, scope_name: &str) -> Option<Option<String>> {
    if scope_name != "livekit-agents" {
        return None;
    }
    let text = bag.get("gen_ai.provider.name").and_then(as_str)?;
    let host = host_of(text);
    let mapped = match host.as_deref() {
        Some("api.openai.com") => Some("openai"),
        Some(host) => match_endpoint(host),
        None => None,
    };
    let is_host =
        text.contains(':') || text.trim().parse::<std::net::IpAddr>().is_ok() || mapped.is_some();
    if !is_host {
        return None;
    }
    bag.take("gen_ai.provider.name");
    Some(mapped.map(str::to_owned))
}

/// Whether `key` is a framework-naming provider key whose value is in
/// [`FRAMEWORK_NAMES`], compared after lowercasing.
fn names_framework(bag: &AttrBag<'_>, key: &str) -> bool {
    matches!(key, "gen_ai.provider.name" | "gen_ai.system")
        && bag.get(key).and_then(as_str).is_some_and(|value| {
            FRAMEWORK_NAMES
                .binary_search(&value.to_lowercase().as_str())
                .is_ok()
        })
}

/// D-IN-23 item 6: on Mirascope spans, a model written `<provider>/<model>` is
/// stored without that prefix, so cost matches the bare name. The provider is
/// the raw lowercased `gen_ai.provider.name`, read before rule 12 aliases it.
/// Nothing is stripped on another scope, or when the prefix differs from the
/// provider (OpenRouter's `vendor/model`).
fn mirascope_model(bag: &AttrBag<'_>, scope_name: &str, model: Option<String>) -> Option<String> {
    let model = model?;
    if !scope_name.starts_with("mirascope") {
        return Some(model);
    }
    let Some(provider) = bag
        .get("gen_ai.provider.name")
        .and_then(as_str)
        .filter(|provider| !provider.is_empty())
    else {
        return Some(model);
    };
    let provider = provider.to_lowercase();
    let bare = model
        .strip_prefix(provider.as_str())
        .and_then(|rest| rest.strip_prefix('/'));
    match bare {
        Some(bare) if !bare.is_empty() => Some(bare.to_owned()),
        _ => Some(model),
    }
}

/// V3 (D-IN-6): on a `voltagent` span whose model is still empty, `llm.model`
/// (the `llm` span), then `ai.model.name` (the agent span), fill it. Not rows
/// in [`REQUEST_MODEL`]: OpenInference and OpenLLMetry use the `llm.` namespace
/// too, so the keys are read only here (D-IN-23). A key is consumed when it
/// fills the column.
fn voltagent_model(
    bag: &mut AttrBag<'_>,
    conventions: &[Convention],
    model: Option<String>,
) -> Option<String> {
    if model.is_some() || !conventions.contains(&Convention::Voltagent) {
        return model;
    }
    for key in ["llm.model", "ai.model.name"] {
        if let Some(text) = bag
            .get(key)
            .and_then(as_str)
            .filter(|text| !text.is_empty())
        {
            bag.take(key);
            return Some(text.to_owned());
        }
    }
    model
}

/// R-PF-2 (D-IN-5): on scope `promptflow`, only on a span whose `span_type` is
/// `LLM` or `Embedding`. An empty response model is filled from
/// `llm.response.model`, an empty request model from the `model` field of the
/// `inputs` JSON. A key is consumed when it fills its column. `inputs` is
/// never consumed here: it is the span's input. `span_type` is peeked, since
/// the kind row has already consumed it.
fn promptflow_models(
    bag: &mut AttrBag<'_>,
    scope_name: &str,
    request: Option<String>,
    response: Option<String>,
    warnings: &mut Warnings,
) -> (Option<String>, Option<String>) {
    let model_span = bag
        .get("span_type")
        .and_then(as_str)
        .is_some_and(|text| matches!(text, "LLM" | "Embedding"));
    if scope_name != "promptflow" || !model_span {
        return (request, response);
    }
    let response = response.or_else(|| {
        let text = bag
            .get("llm.response.model")
            .and_then(as_str)
            .filter(|text| !text.is_empty())?
            .to_owned();
        bag.take("llm.response.model");
        Some(text)
    });
    let request = request.or_else(|| {
        let value = bag.get("inputs")?;
        let json = crate::payload::decode_json_text(value, warnings)?;
        let text = json
            .get("model")?
            .as_str()
            .filter(|text| !text.is_empty())?
            .to_owned();
        Some(text)
    });
    (request, response)
}

/// R-PC-3 (D-IN-5): on a span of scope `pipecat.turn` whose session is still
/// empty, `conversation.id` fills it. Only the `conversation` and `turn` spans
/// carry the key, so the scope is exact. A key is consumed when it fills the column.
fn pipecat_session(
    bag: &mut AttrBag<'_>,
    scope_name: &str,
    session: Option<String>,
) -> Option<String> {
    if session.is_some() || scope_name != "pipecat.turn" {
        return session;
    }
    let text = bag
        .get("conversation.id")
        .and_then(as_str)
        .filter(|text| !text.is_empty())?;
    let text = text.to_owned();
    bag.take("conversation.id");
    Some(text)
}

/// V6 (D-IN-6): on a `voltagent` span whose session is still empty,
/// `conversation.id`, else `voltagent.conversation_id`, fills it. Not rows in
/// [`SESSION_ID`]: the keys are VoltAgent's, and read only on its spans.
fn voltagent_session(
    bag: &mut AttrBag<'_>,
    conventions: &[Convention],
    session: Option<String>,
) -> Option<String> {
    if session.is_some() || !conventions.contains(&Convention::Voltagent) {
        return session;
    }
    for key in ["conversation.id", "voltagent.conversation_id"] {
        if let Some(text) = bag
            .get(key)
            .and_then(as_str)
            .filter(|text| !text.is_empty())
        {
            bag.take(key);
            return Some(text.to_owned());
        }
    }
    session
}

/// The provider after the endpoint rule (D-IN-2, spec 02 § Provider from the endpoint).
/// `provider` is the value `fill` resolved. Only `llm` and `embedding` spans whose
/// provider is empty or `openai` change; the host keys stay in `attributes`.
pub(crate) fn endpoint_provider(
    bag: &AttrBag<'_>,
    kind: SpanKind,
    provider: Option<String>,
) -> Option<String> {
    if !matches!(kind, SpanKind::Llm | SpanKind::Embedding) {
        return provider;
    }
    if provider.as_deref().is_some_and(|value| value != "openai") {
        return provider;
    }
    let host = HOST_KEYS
        .iter()
        .find_map(|key| bag.get(key).and_then(as_str).and_then(host_of));
    match host.as_deref().and_then(match_endpoint) {
        Some(name) => Some(name.to_owned()),
        None => provider,
    }
}

/// The lowercased host of a bare host or a URL: no scheme, user-info, port, path,
/// query, fragment or trailing dot. `None` when nothing is left.
fn host_of(value: &str) -> Option<String> {
    let rest = value.trim();
    let rest = match rest.find("://") {
        Some(index) if !rest[..index].contains(['/', '?', '#']) => &rest[index + 3..],
        _ => rest,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed
            .split_once(']')
            .map_or(bracketed, |(inside, _)| inside)
    } else {
        authority
            .split_once(':')
            .map_or(authority, |(name, _)| name)
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

fn match_endpoint(host: &str) -> Option<&'static str> {
    ENDPOINT_PROVIDERS.iter().find_map(|(pattern, provider)| {
        let matched = match pattern.strip_prefix("*.") {
            Some(suffix) => host
                .strip_suffix(suffix)
                .is_some_and(|head| head.len() > 1 && head.ends_with('.')),
            None => host == *pattern,
        };
        matched.then_some(*provider)
    })
}

fn string_field(
    bag: &mut AttrBag<'_>,
    resource: &ResourceView<'_>,
    sources: &[Source],
) -> Option<String> {
    first_string(bag, resource, sources).map(|resolved| resolved.value)
}

/// `value` after rule 12: lowercased, then mapped through the alias table. Cost also maps a
/// model's `prefix/` with it (D-IN-33).
pub(crate) fn canonical_provider(value: &str, from_vercel: bool) -> String {
    let lower = value.to_lowercase();
    if lower.starts_with("vertex_ai-") {
        return "gcp.vertex_ai".to_owned();
    }
    if let Some(alias) = exact_alias(&lower) {
        return alias.to_owned();
    }
    if WELL_KNOWN_PROVIDERS.binary_search(&lower.as_str()).is_ok() {
        return lower;
    }
    if from_vercel {
        return vercel_provider(&lower);
    }
    lower
}

/// Provider names that instrumentations write for a semconv name. Checked before the
/// well-known list. `google` is `gcp.gen_ai` (any Google GenAI endpoint), not one platform.
fn exact_alias(value: &str) -> Option<&'static str> {
    Some(match value {
        "openai.chat" | "openai.responses" | "openai:completions" | "openai:responses" => "openai",
        // Quarkus LangChain4j (D-IN-30).
        "open_ai" => "openai",
        "anthropic.messages" => "anthropic",
        "xai" => "x_ai",
        "gemini" => "gcp.gemini",
        "google" => "gcp.gen_ai",
        "vertexai" | "vertex_ai" => "gcp.vertex_ai",
        "azure" | "az.ai.openai" => "azure.ai.openai",
        "az.ai.inference" => "azure.ai.inference",
        "mistralai" => "mistral_ai",
        "aws" => "aws.bedrock",
        "fireworks" => "fireworks_ai",
        "together" | "togetherai" => "together_ai",
        "watsonx" | "ibm" => "ibm.watsonx.ai",
        "oracle" => "oci",
        "bedrock" => "aws.bedrock",
        "azure_ai" => "azure.ai.inference",
        "mistral" => "mistral_ai",
        "ollama_chat" => "ollama",
        "cohere_chat" => "cohere",
        "text-completion-openai" => "openai",
        "clf_ai_gateway" => "cloudflare_ai_gateway",
        _ => return None,
    })
}

fn vercel_provider(value: &str) -> String {
    if value == "google.generative-ai" {
        return "gcp.gemini".to_owned();
    }
    if value.starts_with("google.vertex.") {
        return "gcp.vertex_ai".to_owned();
    }
    if value == "amazon-bedrock" {
        return "aws.bedrock".to_owned();
    }
    if value.starts_with("azure.") {
        return "azure.ai.openai".to_owned();
    }
    if value.starts_with("xai.") {
        return "x_ai".to_owned();
    }
    if value.starts_with("mistral.") {
        return "mistral_ai".to_owned();
    }
    match value.split_once('.') {
        Some((name, _)) if !name.is_empty() => name.to_owned(),
        _ => value.to_owned(),
    }
}

fn request_params(bag: &mut AttrBag<'_>, warnings: &mut Warnings) -> Option<String> {
    let mut params = BTreeMap::new();
    for key in prefixed(bag, "gen_ai.request.") {
        if key == "gen_ai.request.model" {
            continue;
        }
        let Some(name) = key.strip_prefix("gen_ai.request.") else {
            continue;
        };
        insert_param(bag, &key, name, &mut params, warnings);
    }
    expand_param_object(bag, "llm.invocation_parameters", &mut params);
    for key in prefixed(bag, "ai.settings.") {
        // v7 runtime context is session, user, and metadata (D-B1-5).
        if key.starts_with(VERCEL_CONTEXT) {
            continue;
        }
        let Some(name) = key.strip_prefix("ai.settings.") else {
            continue;
        };
        insert_param(bag, &key, name, &mut params, warnings);
    }
    expand_param_object(bag, "langfuse.observation.model.parameters", &mut params);
    if params.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&params).unwrap_or_else(|_| "{}".to_owned()))
    }
}

fn insert_param(
    bag: &mut AttrBag<'_>,
    key: &str,
    name: &str,
    params: &mut BTreeMap<String, Json>,
    warnings: &mut Warnings,
) {
    if name.is_empty() {
        return;
    }
    let Some(value) = bag.get(key) else {
        return;
    };
    if !params.contains_key(name) {
        params.insert(name.to_owned(), to_json(value, warnings));
    }
    bag.take(key);
}

/// A JSON object string is expanded and consumed. Any other value stays unread.
fn expand_param_object(bag: &mut AttrBag<'_>, key: &str, params: &mut BTreeMap<String, Json>) {
    let Some(text) = bag.get(key).and_then(as_str) else {
        return;
    };
    let Ok(Json::Object(entries)) = serde_json::from_str::<Json>(text) else {
        return;
    };
    for (name, value) in entries {
        if !name.is_empty() && !params.contains_key(&name) {
            params.insert(name, value);
        }
    }
    bag.take(key);
}

fn tags(bag: &mut AttrBag<'_>) -> Vec<String> {
    for source in TAGS {
        let Some(value) = bag.get(source.key) else {
            continue;
        };
        let Some(items) = as_string_list(value) else {
            bag.take(source.key);
            continue;
        };
        let tags = dedup(items);
        bag.take(source.key);
        if !tags.is_empty() {
            return tags;
        }
    }
    Vec::new()
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut tags = Vec::new();
    for item in items {
        if item.is_empty() || tags.iter().any(|existing| existing == &item) {
            continue;
        }
        tags.push(item);
    }
    tags
}

fn metadata(bag: &mut AttrBag<'_>, warnings: &mut Warnings) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();
    sweep_prefix(bag, attr::METADATA_PREFIX, &mut metadata, warnings);
    expand_metadata_object(bag, "metadata", &mut metadata);
    sweep_prefix(bag, "ai.telemetry.metadata.", &mut metadata, warnings);
    sweep_prefix(bag, VERCEL_CONTEXT, &mut metadata, warnings);
    function_id(bag, &mut metadata, warnings);
    sweep_prefix(
        bag,
        "traceloop.association.properties.",
        &mut metadata,
        warnings,
    );
    workflow_name(bag, &mut metadata, warnings);
    sweep_prefix(bag, "langfuse.trace.metadata.", &mut metadata, warnings);
    sweep_prefix(
        bag,
        "lmnr.association.properties.metadata.",
        &mut metadata,
        warnings,
    );
    link(
        bag,
        attr::EXPERIMENT_ID,
        metadata_key::EXPERIMENT_ID,
        &mut metadata,
        warnings,
    );
    link(
        bag,
        attr::DATASET_ITEM_ID,
        metadata_key::DATASET_ITEM_ID,
        &mut metadata,
        warnings,
    );
    metadata
}

fn sweep_prefix(
    bag: &mut AttrBag<'_>,
    prefix: &str,
    metadata: &mut BTreeMap<String, String>,
    warnings: &mut Warnings,
) {
    for key in prefixed(bag, prefix) {
        if skipped(bag, &key) {
            continue;
        }
        let Some(name) = key.strip_prefix(prefix) else {
            continue;
        };
        if name.is_empty() || metadata.contains_key(name) {
            continue;
        }
        let Some(value) = bag.get(&key) else {
            continue;
        };
        if as_str(value).is_some_and(str::is_empty) {
            continue;
        }
        let text = to_map_string(value, warnings);
        bag.take(&key);
        metadata.insert(name.to_owned(), text);
    }
}

fn workflow_name(
    bag: &mut AttrBag<'_>,
    metadata: &mut BTreeMap<String, String>,
    warnings: &mut Warnings,
) {
    if metadata.contains_key("workflow") || bag.get("traceloop.workflow.name").is_none() {
        return;
    }
    link(
        bag,
        "traceloop.workflow.name",
        "workflow",
        metadata,
        warnings,
    );
}

fn function_id(
    bag: &mut AttrBag<'_>,
    metadata: &mut BTreeMap<String, String>,
    warnings: &mut Warnings,
) {
    if metadata.contains_key("functionId") || bag.get("ai.telemetry.functionId").is_none() {
        return;
    }
    link(
        bag,
        "ai.telemetry.functionId",
        "functionId",
        metadata,
        warnings,
    );
}

fn expand_metadata_object(
    bag: &mut AttrBag<'_>,
    key: &str,
    metadata: &mut BTreeMap<String, String>,
) {
    if skipped(bag, key) {
        return;
    }
    let Some(text) = bag.get(key).and_then(as_str) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    let Ok(Json::Object(entries)) = serde_json::from_str::<Json>(text) else {
        return;
    };
    for (name, value) in entries {
        if name.is_empty() || metadata.contains_key(&name) || value.is_null() {
            continue;
        }
        let text = match value {
            Json::String(text) => text,
            other => serde_json::to_string(&other).unwrap_or_else(|_| "null".to_owned()),
        };
        metadata.insert(name, text);
    }
    bag.take(key);
}

fn link(
    bag: &mut AttrBag<'_>,
    source: &str,
    name: &str,
    metadata: &mut BTreeMap<String, String>,
    warnings: &mut Warnings,
) {
    let Some(value) = bag.get(source) else {
        return;
    };
    if as_str(value).is_some_and(str::is_empty) {
        return;
    }
    let text = to_map_string(value, warnings);
    bag.take(source);
    metadata.insert(name.to_owned(), text);
}

fn skipped(bag: &AttrBag<'_>, key: &str) -> bool {
    matches!(bag.state(key), Some(Use::Consumed | Use::Payload)) || is_metadata_context_key(key)
}

fn prefixed(bag: &AttrBag<'_>, prefix: &str) -> Vec<String> {
    bag.keys_with_prefix(prefix).map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::any_value::Value;
    use opentelemetry_proto::tonic::common::v1::{AnyValue, ArrayValue, KeyValue};

    fn string_attr(key: &str, text: &str) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::StringValue(text.to_owned())),
            }),
            ..KeyValue::default()
        }
    }

    fn int_attr(key: &str, number: i64) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::IntValue(number)),
            }),
            ..KeyValue::default()
        }
    }

    fn double_attr(key: &str, number: f64) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::DoubleValue(number)),
            }),
            ..KeyValue::default()
        }
    }

    fn list_attr(key: &str, items: &[&str]) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::ArrayValue(ArrayValue {
                    values: items
                        .iter()
                        .map(|item| AnyValue {
                            value: Some(Value::StringValue((*item).to_owned())),
                        })
                        .collect(),
                })),
            }),
            ..KeyValue::default()
        }
    }

    fn bool_attr(key: &str, flag: bool) -> KeyValue {
        KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue {
                value: Some(Value::BoolValue(flag)),
            }),
            ..KeyValue::default()
        }
    }

    fn filled<'a>(
        attrs: &'a [KeyValue],
        resource_attrs: &[KeyValue],
    ) -> (ContextFields, AttrBag<'a>) {
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(attrs, &mut warnings);
        let view = ResourceView::new(resource_attrs);
        let fields = fill(&mut bag, &view, "", &[], &mut warnings);
        (fields, bag)
    }

    fn filled_with_warnings(attrs: &[KeyValue]) -> (ContextFields, AttrBag<'_>, Warnings) {
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(attrs, &mut warnings);
        let view = ResourceView::new(&[]);
        let fields = fill(&mut bag, &view, "", &[], &mut warnings);
        (fields, bag, warnings)
    }

    fn filled_in_scope<'a>(scope: &str, attrs: &'a [KeyValue]) -> (ContextFields, AttrBag<'a>) {
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(attrs, &mut warnings);
        let view = ResourceView::new(&[]);
        let fields = fill(&mut bag, &view, scope, &[], &mut warnings);
        (fields, bag)
    }

    fn filled_as<'a>(
        scope: &str,
        attrs: &'a [KeyValue],
        conventions: &[Convention],
    ) -> (ContextFields, AttrBag<'a>) {
        let mut warnings = Warnings::default();
        let mut bag = AttrBag::new(attrs, &mut warnings);
        let view = ResourceView::new(&[]);
        let fields = fill(&mut bag, &view, scope, conventions, &mut warnings);
        (fields, bag)
    }

    #[test]
    fn prompt_from_cetra() {
        let attrs = [
            string_attr(attr::PROMPT_NAME, "support-reply"),
            int_attr(attr::PROMPT_VERSION, 2),
        ];
        let (fields, bag, warnings) = filled_with_warnings(&attrs);
        assert_eq!(fields.prompt_name.as_deref(), Some("support-reply"));
        assert_eq!(fields.prompt_version, Some(2));
        assert_eq!(bag.state(attr::PROMPT_NAME), Some(Use::Consumed));
        assert_eq!(bag.state(attr::PROMPT_VERSION), Some(Use::Consumed));
        assert_eq!(warnings.to_attribute(), None);
    }

    #[test]
    fn prompt_version_as_string_and_double() {
        let (fields, _) = filled(
            &[
                string_attr(attr::PROMPT_NAME, "p"),
                string_attr(attr::PROMPT_VERSION, "7"),
            ],
            &[],
        );
        assert_eq!(fields.prompt_version, Some(7));
        let (fields, _) = filled(
            &[
                string_attr(attr::PROMPT_NAME, "p"),
                double_attr(attr::PROMPT_VERSION, 7.0),
            ],
            &[],
        );
        assert_eq!(fields.prompt_version, Some(7));
    }

    #[test]
    fn cetra_name_wins_and_pairs_its_version() {
        let attrs = [
            string_attr(attr::PROMPT_NAME, "support-reply"),
            string_attr("langfuse.observation.prompt.name", "other"),
            int_attr("langfuse.observation.prompt.version", 5),
        ];
        let (fields, bag, warnings) = filled_with_warnings(&attrs);
        assert_eq!(fields.prompt_name.as_deref(), Some("support-reply"));
        assert_eq!(fields.prompt_version, None);
        assert_eq!(
            bag.state("langfuse.observation.prompt.name"),
            Some(Use::Unread)
        );
        assert_eq!(
            bag.state("langfuse.observation.prompt.version"),
            Some(Use::Unread)
        );
        assert_eq!(warnings.to_attribute(), None);
    }

    #[test]
    fn langfuse_pair() {
        let attrs = [
            string_attr("langfuse.observation.prompt.name", "summarize"),
            string_attr("langfuse.observation.prompt.version", "3"),
        ];
        let (fields, bag, warnings) = filled_with_warnings(&attrs);
        assert_eq!(fields.prompt_name.as_deref(), Some("summarize"));
        assert_eq!(fields.prompt_version, Some(3));
        assert_eq!(
            bag.state("langfuse.observation.prompt.name"),
            Some(Use::Consumed)
        );
        assert_eq!(
            bag.state("langfuse.observation.prompt.version"),
            Some(Use::Consumed)
        );
        assert_eq!(warnings.to_attribute(), None);
    }

    #[test]
    fn invalid_versions() {
        let versions = [
            int_attr(attr::PROMPT_VERSION, 0),
            int_attr(attr::PROMPT_VERSION, -1),
            double_attr(attr::PROMPT_VERSION, 1.5),
            string_attr(attr::PROMPT_VERSION, "x"),
            bool_attr(attr::PROMPT_VERSION, true),
        ];
        for version in versions {
            let attrs = [string_attr(attr::PROMPT_NAME, "p"), version];
            let (fields, bag, warnings) = filled_with_warnings(&attrs);
            assert_eq!(fields.prompt_name.as_deref(), Some("p"));
            assert_eq!(fields.prompt_version, None);
            assert_eq!(bag.state(attr::PROMPT_VERSION), Some(Use::Unread));
            assert_eq!(
                warnings.to_attribute().as_deref(),
                Some("[\"invalid_prompt_version\"]")
            );
        }
    }

    #[test]
    fn version_without_name() {
        let attrs = [
            int_attr(attr::PROMPT_VERSION, 4),
            int_attr("langfuse.observation.prompt.version", 5),
        ];
        let (fields, bag, warnings) = filled_with_warnings(&attrs);
        assert_eq!(fields.prompt_name, None);
        assert_eq!(fields.prompt_version, None);
        assert_eq!(bag.state(attr::PROMPT_VERSION), Some(Use::Unread));
        assert_eq!(
            bag.state("langfuse.observation.prompt.version"),
            Some(Use::Unread)
        );
        assert_eq!(
            warnings.to_attribute().as_deref(),
            Some("[\"prompt_version_without_name\"]")
        );
    }

    #[test]
    fn empty_name_is_absent() {
        for name in ["", "  "] {
            let attrs = [string_attr(attr::PROMPT_NAME, name)];
            let (fields, bag, warnings) = filled_with_warnings(&attrs);
            assert_eq!(fields.prompt_name, None, "{name:?}");
            assert_eq!(fields.prompt_version, None);
            assert_eq!(bag.state(attr::PROMPT_NAME), Some(Use::Unread));
            assert_eq!(warnings.to_attribute(), None);
        }
    }

    #[test]
    fn resource_keys_are_ignored() {
        let (fields, _) = filled(&[], &[string_attr(attr::PROMPT_NAME, "p")]);
        assert_eq!(fields.prompt_name, None);
    }

    #[test]
    fn well_known_providers_are_sorted() {
        assert!(WELL_KNOWN_PROVIDERS.is_sorted());
    }

    #[test]
    fn framework_names_are_sorted() {
        assert!(FRAMEWORK_NAMES.is_sorted());
        assert!(FRAMEWORK_NAMES.windows(2).all(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn framework_names_in_gen_ai_system_are_skipped() {
        let attrs = [string_attr("gen_ai.system", "autogen")];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.provider, None);
        assert_eq!(bag.state("gen_ai.system"), Some(Use::Unread));

        let (fields, _) = filled(&[string_attr("gen_ai.system", "AutoGen")], &[]);
        assert_eq!(fields.provider, None);

        let attrs = [
            string_attr("gen_ai.system", "spring_ai"),
            string_attr("llm.provider", "openai"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.provider.as_deref(), Some("openai"));
        assert_eq!(bag.state("gen_ai.system"), Some(Use::Unread));
    }

    #[test]
    fn framework_names_in_gen_ai_provider_name_are_skipped_too() {
        for name in [
            "strands-agents",
            "microsoft.agent_framework",
            "Strands-Agents",
        ] {
            let attrs = [string_attr("gen_ai.provider.name", name)];
            let (fields, bag) = filled(&attrs, &[]);
            assert_eq!(fields.provider, None, "{name}");
            assert_eq!(bag.state("gen_ai.provider.name"), Some(Use::Unread));
        }
        // Left unread, so the next source decides.
        let attrs = [
            string_attr("gen_ai.provider.name", "strands-agents"),
            string_attr("gen_ai.system", "gcp.vertex.agent"),
            string_attr("llm.provider", "openai"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.provider.as_deref(), Some("openai"));
        assert_eq!(bag.state("gen_ai.provider.name"), Some(Use::Unread));
        assert_eq!(bag.state("gen_ai.system"), Some(Use::Unread));
        assert_eq!(bag.state("llm.provider"), Some(Use::Consumed));

        // A framework name on `gen_ai.system` behind a real provider on
        // `gen_ai.provider.name` keeps the provider.
        let attrs = [
            string_attr("gen_ai.provider.name", "openai"),
            string_attr("gen_ai.system", "microsoft.agent_framework"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.provider.as_deref(), Some("openai"));
        assert_eq!(bag.state("gen_ai.provider.name"), Some(Use::Consumed));
    }

    #[test]
    fn browser_use_is_a_framework_name_never_a_provider() {
        // R-OL-3 (D-IN-15): OpenLIT writes `gen_ai.provider.name=browser_use`
        // on its Browser Use spans. It is skipped and left unread.
        for (key, value) in [
            ("gen_ai.provider.name", "browser_use"),
            ("gen_ai.provider.name", "Browser_Use"),
            ("gen_ai.system", "browser_use"),
        ] {
            let attrs = [string_attr(key, value)];
            let (fields, bag) = filled(&attrs, &[]);
            assert_eq!(fields.provider, None, "{key}={value}");
            assert_eq!(bag.state(key), Some(Use::Unread), "{key}={value}");
        }
        // The next source decides.
        let attrs = [
            string_attr("gen_ai.provider.name", "browser_use"),
            string_attr("llm.provider", "openai"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.provider.as_deref(), Some("openai"));
        assert_eq!(bag.state("gen_ai.provider.name"), Some(Use::Unread));
        assert_eq!(bag.state("llm.provider"), Some(Use::Consumed));
    }

    #[test]
    fn koog_is_a_framework_not_a_provider() {
        // D-IN-30 item 4: Koog names itself as the provider on its tool spans.
        for (key, value) in [
            ("gen_ai.provider.name", "koog"),
            ("gen_ai.provider.name", "Koog"),
            ("gen_ai.system", "koog"),
        ] {
            let attrs = [string_attr(key, value)];
            let (fields, bag) = filled(&attrs, &[]);
            assert_eq!(fields.provider, None, "{key}={value}");
            assert_eq!(bag.state(key), Some(Use::Unread), "{key}={value}");
        }
        // Left unread, so the next source decides.
        let attrs = [
            string_attr("gen_ai.provider.name", "koog"),
            string_attr("llm.provider", "openai"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.provider.as_deref(), Some("openai"));
        assert_eq!(bag.state("gen_ai.provider.name"), Some(Use::Unread));
        assert_eq!(bag.state("llm.provider"), Some(Use::Consumed));
    }

    /// The endpoint rule over one span: the attributes, its kind and the provider
    /// `fill` resolved.
    fn endpoint(attrs: &[KeyValue], kind: SpanKind, provider: Option<&str>) -> Option<String> {
        let mut warnings = Warnings::default();
        let bag = AttrBag::new(attrs, &mut warnings);
        endpoint_provider(&bag, kind, provider.map(str::to_owned))
    }

    #[test]
    fn host_of_reads_hosts_and_urls() {
        assert_eq!(host_of("api.groq.com").as_deref(), Some("api.groq.com"));
        assert_eq!(
            host_of("https://OpenRouter.ai/api/v1/").as_deref(),
            Some("openrouter.ai")
        );
        assert_eq!(
            host_of("http://user:pw@api.novita.ai:8443/openai?x=1#f").as_deref(),
            Some("api.novita.ai")
        );
        assert_eq!(
            host_of("https://x.openai.azure.com./openai").as_deref(),
            Some("x.openai.azure.com")
        );
        assert_eq!(host_of("http://[::1]:11434/v1").as_deref(), Some("::1"));
        assert_eq!(
            host_of("api.groq.com/v1?u=http://x").as_deref(),
            Some("api.groq.com")
        );
        assert_eq!(host_of(""), None);
        assert_eq!(host_of("https://"), None);
    }

    #[test]
    fn match_endpoint_uses_exact_hosts_and_label_suffixes() {
        assert_eq!(match_endpoint("openrouter.ai"), Some("openrouter"));
        assert_eq!(match_endpoint("api.openrouter.ai"), None);
        assert_eq!(
            match_endpoint("res.openai.azure.com"),
            Some("azure.ai.openai")
        );
        assert_eq!(match_endpoint("openai.azure.com"), None);
        assert_eq!(match_endpoint("evilopenai.azure.com"), None);
        assert_eq!(
            match_endpoint("a.b.cloud.databricks.com"),
            Some("databricks")
        );
        assert_eq!(match_endpoint("api.openai.com"), None);
        assert_eq!(match_endpoint("x.maas.aliyuncs.com"), Some("dashscope"));
        assert_eq!(
            match_endpoint("us-central1-aiplatform.googleapis.com"),
            None
        );
        assert_eq!(match_endpoint("model-abc.api.baseten.co"), Some("baseten"));
        assert_eq!(
            match_endpoint("res.services.ai.azure.com"),
            Some("azure.ai.inference")
        );
    }

    #[test]
    fn endpoint_rule_applies_to_llm_and_embedding_spans_only() {
        let attrs = [string_attr("server.address", "api.groq.com")];
        for kind in [SpanKind::Llm, SpanKind::Embedding] {
            assert_eq!(endpoint(&attrs, kind, None).as_deref(), Some("groq"));
        }
        for kind in [SpanKind::Tool, SpanKind::Span] {
            assert_eq!(endpoint(&attrs, kind, None), None);
        }
    }

    #[test]
    fn endpoint_rule_replaces_only_empty_or_openai() {
        let attrs = [string_attr("server.address", "api.groq.com")];
        assert_eq!(
            endpoint(&attrs, SpanKind::Llm, Some("openai")).as_deref(),
            Some("groq")
        );
        assert_eq!(
            endpoint(&attrs, SpanKind::Llm, Some("anthropic")).as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            endpoint(&attrs, SpanKind::Llm, None).as_deref(),
            Some("groq")
        );
    }

    #[test]
    fn endpoint_rule_reads_keys_in_order() {
        let both = [
            string_attr("server.address", "api.groq.com"),
            string_attr("gen_ai.openai.api_base", "https://api.novita.ai/openai/"),
        ];
        assert_eq!(
            endpoint(&both, SpanKind::Llm, None).as_deref(),
            Some("groq")
        );

        let url_only = [string_attr(
            "url.full",
            "https://api.novita.ai/openai/chat/completions",
        )];
        assert_eq!(
            endpoint(&url_only, SpanKind::Llm, None).as_deref(),
            Some("novita")
        );

        let base_only = [string_attr(
            "gen_ai.openai.api_base",
            "https://openrouter.ai/api/v1/",
        )];
        assert_eq!(
            endpoint(&base_only, SpanKind::Llm, None).as_deref(),
            Some("openrouter")
        );

        let local = [string_attr("server.address", "localhost")];
        assert_eq!(
            endpoint(&local, SpanKind::Llm, Some("openai")).as_deref(),
            Some("openai")
        );

        let empty_first = [
            string_attr("server.address", ""),
            string_attr("url.full", "https://api.x.ai/v1"),
        ];
        assert_eq!(
            endpoint(&empty_first, SpanKind::Llm, None).as_deref(),
            Some("x_ai")
        );
    }

    #[test]
    fn endpoint_keys_are_not_consumed() {
        let attrs = [string_attr("server.address", "api.groq.com")];
        let mut warnings = Warnings::default();
        let bag = AttrBag::new(&attrs, &mut warnings);
        assert_eq!(
            endpoint_provider(&bag, SpanKind::Llm, None).as_deref(),
            Some("groq")
        );
        assert_eq!(bag.state("server.address"), Some(Use::Unread));
    }

    #[test]
    fn request_model_uses_precedence() {
        let attrs = [
            string_attr("ai.model.id", "vercel"),
            string_attr("llm.model_name", "openinference"),
            string_attr("gen_ai.request.model", "genai"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.request_model.as_deref(), Some("genai"));
        assert_eq!(bag.state("gen_ai.request.model"), Some(Use::Consumed));
        assert_eq!(bag.state("llm.model_name"), Some(Use::Unread));
        assert_eq!(bag.state("ai.model.id"), Some(Use::Unread));

        let attrs = [
            string_attr("gen_ai.request.model", ""),
            string_attr("embedding.model_name", "embed"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.request_model.as_deref(), Some("embed"));
        assert_eq!(bag.state("gen_ai.request.model"), Some(Use::Unread));
        assert_eq!(bag.state("embedding.model_name"), Some(Use::Consumed));
    }

    #[test]
    fn response_model_uses_precedence() {
        let attrs = [
            string_attr("ai.response.model", "vercel"),
            string_attr("gen_ai.response.model", "genai"),
        ];
        let (fields, _) = filled(&attrs, &[]);
        assert_eq!(fields.response_model.as_deref(), Some("genai"));
        let (fields, _) = filled(&[string_attr("ai.response.model", "vercel")], &[]);
        assert_eq!(fields.response_model.as_deref(), Some("vercel"));
    }

    #[test]
    fn mirascope_models_lose_their_provider_prefix_under_its_scope() {
        let attrs = [
            string_attr("gen_ai.provider.name", "openai"),
            string_attr("gen_ai.request.model", "openai/gpt-4o-mini"),
            string_attr("gen_ai.response.model", "openai/gpt-4o-mini"),
        ];
        let (fields, _) = filled_in_scope("mirascope.llm", &attrs);
        assert_eq!(fields.request_model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(fields.response_model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(fields.provider.as_deref(), Some("openai"));

        // The registered provider id is aliased for the provider column and
        // stripped raw from the model.
        let attrs = [
            string_attr("gen_ai.provider.name", "openai:completions"),
            string_attr("gen_ai.request.model", "openai:completions/gpt-4o-mini"),
        ];
        let (fields, _) = filled_in_scope("mirascope.llm", &attrs);
        assert_eq!(fields.request_model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(fields.provider.as_deref(), Some("openai"));

        // A raw provider spelled with capitals still strips.
        let attrs = [
            string_attr("gen_ai.provider.name", "OpenAI"),
            string_attr("gen_ai.request.model", "openai/gpt-4o-mini"),
        ];
        let (fields, _) = filled_in_scope("mirascope.ops", &attrs);
        assert_eq!(fields.request_model.as_deref(), Some("gpt-4o-mini"));

        // Without the scope the model is stored as read.
        let attrs = [
            string_attr("gen_ai.provider.name", "openai"),
            string_attr("gen_ai.request.model", "openai/gpt-4o-mini"),
        ];
        let (fields, _) = filled_in_scope("", &attrs);
        assert_eq!(fields.request_model.as_deref(), Some("openai/gpt-4o-mini"));
        let (fields, _) = filled_in_scope("other.framework", &attrs);
        assert_eq!(fields.request_model.as_deref(), Some("openai/gpt-4o-mini"));

        // No strip when the prefix differs from the provider, or there is no
        // provider, or the model has no bare name after the slash.
        let attrs = [
            string_attr("gen_ai.provider.name", "groq"),
            string_attr("gen_ai.request.model", "openai/gpt-oss-120b"),
        ];
        let (fields, _) = filled_in_scope("mirascope.llm", &attrs);
        assert_eq!(fields.request_model.as_deref(), Some("openai/gpt-oss-120b"));

        let attrs = [string_attr("gen_ai.request.model", "openai/gpt-4o-mini")];
        let (fields, _) = filled_in_scope("mirascope.llm", &attrs);
        assert_eq!(fields.request_model.as_deref(), Some("openai/gpt-4o-mini"));

        // An empty provider value is no provider.
        let attrs = [
            string_attr("gen_ai.provider.name", ""),
            string_attr("gen_ai.request.model", "/gpt-4o-mini"),
        ];
        let (fields, _) = filled_in_scope("mirascope.llm", &attrs);
        assert_eq!(fields.request_model.as_deref(), Some("/gpt-4o-mini"));

        let attrs = [
            string_attr("gen_ai.provider.name", "openai"),
            string_attr("gen_ai.request.model", "openai/"),
        ];
        let (fields, _) = filled_in_scope("mirascope.llm", &attrs);
        assert_eq!(fields.request_model.as_deref(), Some("openai/"));
    }

    #[test]
    fn operation_is_the_gen_ai_name() {
        let attrs = [string_attr("gen_ai.operation.name", "retrieval")];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.operation.as_deref(), Some("retrieval"));
        assert_eq!(bag.state("gen_ai.operation.name"), Some(Use::Consumed));
        let (fields, _) = filled(&[], &[]);
        assert_eq!(fields.operation, None);
    }

    #[test]
    fn provider_uses_precedence_and_the_alias_table() {
        let attrs = [
            string_attr("ai.model.provider", "anthropic.messages"),
            string_attr("gen_ai.provider.name", "OpenAI"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.provider.as_deref(), Some("openai"));
        assert_eq!(bag.state("gen_ai.provider.name"), Some(Use::Consumed));
        assert_eq!(bag.state("ai.model.provider"), Some(Use::Unread));

        let cases = [
            ("gen_ai.provider.name", "openai.chat", "openai"),
            ("gen_ai.provider.name", "openai.responses", "openai"),
            // Mirascope's registered provider ids.
            ("gen_ai.provider.name", "openai:completions", "openai"),
            ("gen_ai.provider.name", "openai:responses", "openai"),
            ("gen_ai.provider.name", "anthropic.messages", "anthropic"),
            ("gen_ai.provider.name", "xai", "x_ai"),
            ("gen_ai.provider.name", "XAI", "x_ai"),
            ("gen_ai.provider.name", "aws.bedrock", "aws.bedrock"),
            ("gen_ai.provider.name", "AWS.Bedrock", "aws.bedrock"),
            ("gen_ai.provider.name", "custom.vendor", "custom.vendor"),
            ("ai.model.provider", "openai.chat", "openai"),
            ("ai.model.provider", "aws.bedrock", "aws.bedrock"),
            ("ai.model.provider", "custom.vendor", "custom"),
            ("ai.model.provider", "google.generative-ai", "gcp.gemini"),
            (
                "ai.model.provider",
                "google.vertex.anthropic",
                "gcp.vertex_ai",
            ),
            ("ai.model.provider", "amazon-bedrock", "aws.bedrock"),
            ("ai.model.provider", "azure.openai", "azure.ai.openai"),
            ("ai.model.provider", "xai.chat", "x_ai"),
            ("ai.model.provider", "mistral.small", "mistral_ai"),
            ("ai.model.provider", "groq", "groq"),
            ("gen_ai.system", "OpenAI", "openai"),
            // Names that instrumentations write (Google ADK, OpenInference, Azure, AWS).
            ("gen_ai.provider.name", "gemini", "gcp.gemini"),
            ("gen_ai.provider.name", "google", "gcp.gen_ai"),
            ("gen_ai.provider.name", "vertexai", "gcp.vertex_ai"),
            ("gen_ai.provider.name", "vertex_ai", "gcp.vertex_ai"),
            ("gen_ai.provider.name", "azure", "azure.ai.openai"),
            ("gen_ai.provider.name", "az.ai.openai", "azure.ai.openai"),
            (
                "gen_ai.provider.name",
                "az.ai.inference",
                "azure.ai.inference",
            ),
            ("gen_ai.provider.name", "mistralai", "mistral_ai"),
            ("gen_ai.provider.name", "aws", "aws.bedrock"),
            ("ai.model.provider", "gemini", "gcp.gemini"),
            ("ai.model.provider", "azure", "azure.ai.openai"),
            // LiteLLM and gateway names (D-IN-15).
            ("gen_ai.provider.name", "fireworks", "fireworks_ai"),
            ("gen_ai.provider.name", "together", "together_ai"),
            ("gen_ai.provider.name", "togetherai", "together_ai"),
            ("gen_ai.provider.name", "watsonx", "ibm.watsonx.ai"),
            ("gen_ai.provider.name", "ibm", "ibm.watsonx.ai"),
            ("gen_ai.provider.name", "oracle", "oci"),
            ("gen_ai.provider.name", "bedrock", "aws.bedrock"),
            ("gen_ai.provider.name", "azure_ai", "azure.ai.inference"),
            ("gen_ai.provider.name", "mistral", "mistral_ai"),
            ("gen_ai.provider.name", "ollama_chat", "ollama"),
            ("gen_ai.provider.name", "cohere_chat", "cohere"),
            ("gen_ai.provider.name", "text-completion-openai", "openai"),
            (
                "gen_ai.provider.name",
                "clf_ai_gateway",
                "cloudflare_ai_gateway",
            ),
            (
                "gen_ai.provider.name",
                "vertex_ai-anthropic_models",
                "gcp.vertex_ai",
            ),
        ];
        for (key, raw, expected) in cases {
            let (fields, _) = filled(&[string_attr(key, raw)], &[]);
            assert_eq!(fields.provider.as_deref(), Some(expected), "{key}={raw}");
        }
    }

    #[test]
    fn open_ai_is_openai() {
        // D-IN-30 item 4: Quarkus LangChain4j writes `open_ai`.
        for key in ["gen_ai.provider.name", "gen_ai.system"] {
            let attrs = [string_attr(key, "open_ai")];
            let (fields, bag) = filled(&attrs, &[]);
            assert_eq!(fields.provider.as_deref(), Some("openai"), "{key}");
            assert_eq!(bag.state(key), Some(Use::Consumed), "{key}");
        }
    }

    #[test]
    fn request_params_keep_json_types_and_precedence() {
        let attrs = [
            double_attr("gen_ai.request.temperature", 0.2),
            int_attr("gen_ai.request.max_tokens", 128),
            string_attr("gen_ai.request.stop", "stop"),
            string_attr("gen_ai.request.model", "demo"),
            int_attr("ai.settings.temperature", 9),
            int_attr("ai.settings.maxRetries", 2),
            string_attr(
                "llm.invocation_parameters",
                r#"{"top_p":0.5,"stop":"later"}"#,
            ),
            string_attr("langfuse.observation.model.parameters", "not-json"),
            string_attr("llm.invocation_parameters.ignored", "{}"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(
            fields.request_params.as_deref(),
            Some(
                r#"{"maxRetries":2,"max_tokens":128,"stop":"stop","temperature":0.2,"top_p":0.5}"#
            )
        );
        assert_eq!(fields.request_model.as_deref(), Some("demo"));
        assert_eq!(bag.state("gen_ai.request.temperature"), Some(Use::Consumed));
        assert_eq!(bag.state("ai.settings.temperature"), Some(Use::Consumed));
        assert_eq!(bag.state("ai.settings.maxRetries"), Some(Use::Consumed));
        assert_eq!(bag.state("llm.invocation_parameters"), Some(Use::Consumed));
        assert_eq!(
            bag.state("langfuse.observation.model.parameters"),
            Some(Use::Unread)
        );
        assert_eq!(bag.state("gen_ai.request.model"), Some(Use::Consumed));
    }

    #[test]
    fn session_id_and_user_id_use_precedence() {
        let attrs = [
            string_attr("langfuse.session.id", "langfuse"),
            string_attr("session.id", "otel"),
            string_attr(attr::SESSION_ID, "ours"),
            string_attr("enduser.id", "end"),
            string_attr("user.id", "user"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.session_id.as_deref(), Some("ours"));
        assert_eq!(fields.user_id.as_deref(), Some("user"));
        assert_eq!(bag.state("session.id"), Some(Use::Unread));
        assert_eq!(bag.state("enduser.id"), Some(Use::Unread));

        let (fields, _) = filled(&[string_attr("gen_ai.conversation.id", "conv")], &[]);
        assert_eq!(fields.session_id.as_deref(), Some("conv"));

        // Mirascope's session is the last source, on every Mirascope span.
        let attrs = [
            string_attr("langfuse.session.id", "langfuse"),
            string_attr("mirascope.ops.session.id", "sess-1"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.session_id.as_deref(), Some("langfuse"));
        assert_eq!(bag.state("mirascope.ops.session.id"), Some(Use::Unread));

        let attrs = [string_attr("mirascope.ops.session.id", "sess-1")];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.session_id.as_deref(), Some("sess-1"));
        assert_eq!(bag.state("mirascope.ops.session.id"), Some(Use::Consumed));
    }

    #[test]
    fn voltagent_model_fills_an_empty_request_model_on_voltagent_spans() {
        // V3 (D-IN-6): `llm.model` on the `llm` span, `ai.model.name` on the
        // agent span, each after the sources of the table.
        const VOLTAGENT: &[Convention] = &[Convention::Voltagent];
        let attrs = [string_attr("llm.model", "gpt-4o-mini")];
        let (fields, bag) = filled_as("@voltagent/core", &attrs, VOLTAGENT);
        assert_eq!(fields.request_model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(bag.state("llm.model"), Some(Use::Consumed));

        let attrs = [string_attr("ai.model.name", "gpt-4o-mini")];
        let (fields, bag) = filled_as("@voltagent/core", &attrs, VOLTAGENT);
        assert_eq!(fields.request_model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(bag.state("ai.model.name"), Some(Use::Consumed));
        // Neither is a response model.
        assert_eq!(fields.response_model, None);

        // `llm.model` comes first; an empty value is skipped.
        let attrs = [
            string_attr("llm.model", "gpt-oss-120b"),
            string_attr("ai.model.name", "gpt-4o-mini"),
        ];
        let (fields, bag) = filled_as("@voltagent/core", &attrs, VOLTAGENT);
        assert_eq!(fields.request_model.as_deref(), Some("gpt-oss-120b"));
        assert_eq!(bag.state("ai.model.name"), Some(Use::Unread));

        let attrs = [
            string_attr("llm.model", ""),
            string_attr("ai.model.name", "gpt-4o-mini"),
        ];
        let (fields, bag) = filled_as("@voltagent/core", &attrs, VOLTAGENT);
        assert_eq!(fields.request_model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(bag.state("llm.model"), Some(Use::Unread));

        // The table's sources win, and leave the VoltAgent keys unread.
        let attrs = [
            string_attr("gen_ai.request.model", "genai-model"),
            string_attr("llm.model", "gpt-4o-mini"),
        ];
        let (fields, bag) = filled_as("@voltagent/core", &attrs, VOLTAGENT);
        assert_eq!(fields.request_model.as_deref(), Some("genai-model"));
        assert_eq!(bag.state("llm.model"), Some(Use::Unread));

        let attrs = [
            string_attr("llm.model_name", "openinference-model"),
            string_attr("llm.model", "gpt-4o-mini"),
        ];
        let (fields, bag) = filled_as("@voltagent/core", &attrs, VOLTAGENT);
        assert_eq!(fields.request_model.as_deref(), Some("openinference-model"));
        assert_eq!(bag.state("llm.model"), Some(Use::Unread));

        // Without the `voltagent` convention `llm.model` is no model source:
        // the span stays OpenInference (D-IN-23).
        let attrs = [
            string_attr("llm.model", "gpt-4o-mini"),
            string_attr("ai.model.name", "gpt-4o-mini"),
        ];
        for conventions in [vec![], vec![Convention::Openinference, Convention::Vercel]] {
            let (fields, bag) = filled_as("other.framework", &attrs, &conventions);
            assert_eq!(fields.request_model, None);
            assert_eq!(bag.state("llm.model"), Some(Use::Unread));
            assert_eq!(bag.state("ai.model.name"), Some(Use::Unread));
        }
    }

    #[test]
    fn voltagent_session_fills_an_empty_session_on_voltagent_spans() {
        // V6 (D-IN-6): `conversation.id`, else `voltagent.conversation_id`.
        const VOLTAGENT: &[Convention] = &[Convention::Voltagent];
        let attrs = [string_attr("conversation.id", "conv-1")];
        let (fields, bag) = filled_as("@voltagent/core", &attrs, VOLTAGENT);
        assert_eq!(fields.session_id.as_deref(), Some("conv-1"));
        assert_eq!(bag.state("conversation.id"), Some(Use::Consumed));

        let attrs = [string_attr("voltagent.conversation_id", "conv-2")];
        let (fields, bag) = filled_as("@voltagent/core", &attrs, VOLTAGENT);
        assert_eq!(fields.session_id.as_deref(), Some("conv-2"));
        assert_eq!(bag.state("voltagent.conversation_id"), Some(Use::Consumed));

        let attrs = [
            string_attr("conversation.id", "conv-1"),
            string_attr("voltagent.conversation_id", "conv-2"),
        ];
        let (fields, bag) = filled_as("@voltagent/core", &attrs, VOLTAGENT);
        assert_eq!(fields.session_id.as_deref(), Some("conv-1"));
        assert_eq!(bag.state("voltagent.conversation_id"), Some(Use::Unread));

        // The table's sources win.
        let attrs = [
            string_attr("session.id", "otel-session"),
            string_attr("conversation.id", "conv-1"),
        ];
        let (fields, bag) = filled_as("@voltagent/core", &attrs, VOLTAGENT);
        assert_eq!(fields.session_id.as_deref(), Some("otel-session"));
        assert_eq!(bag.state("conversation.id"), Some(Use::Unread));

        // Without the `voltagent` convention neither key is a session source.
        let attrs = [
            string_attr("conversation.id", "conv-1"),
            string_attr("voltagent.conversation_id", "conv-2"),
        ];
        let (fields, bag) = filled_as("@voltagent/core", &attrs, &[Convention::Openinference]);
        assert_eq!(fields.session_id, None);
        assert_eq!(bag.state("conversation.id"), Some(Use::Unread));
        assert_eq!(bag.state("voltagent.conversation_id"), Some(Use::Unread));
    }

    #[test]
    fn tags_use_the_first_list_and_consume_other_forms() {
        let attrs = [
            string_attr("tag.tags", "red,blue"),
            string_attr(attr::TAGS, r#"["alpha","beta","alpha",""]"#),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.tags, vec!["alpha".to_owned(), "beta".to_owned()]);
        assert_eq!(bag.state(attr::TAGS), Some(Use::Consumed));
        assert_eq!(bag.state("tag.tags"), Some(Use::Unread));

        let attrs = [string_attr("tag.tags", "red,blue")];
        let (fields, bag) = filled(&attrs, &[]);
        assert!(fields.tags.is_empty());
        assert_eq!(bag.state("tag.tags"), Some(Use::Consumed));

        let attrs = [
            string_attr("tag.tags", "red,blue"),
            list_attr("langfuse.trace.tags", &["red", "green"]),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.tags, vec!["red".to_owned(), "green".to_owned()]);
        assert_eq!(bag.state("tag.tags"), Some(Use::Consumed));
        assert_eq!(bag.state("langfuse.trace.tags"), Some(Use::Consumed));
    }

    #[test]
    fn function_id_fills_metadata_unless_that_key_is_already_set() {
        let attrs = [string_attr("ai.telemetry.functionId", "greet")];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(
            fields.metadata.get("functionId").map(String::as_str),
            Some("greet")
        );
        assert_eq!(bag.state("ai.telemetry.functionId"), Some(Use::Consumed));

        let attrs = [
            string_attr("ai.telemetry.metadata.functionId", "from-metadata"),
            string_attr("ai.telemetry.functionId", "greet"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(
            fields.metadata.get("functionId").map(String::as_str),
            Some("from-metadata")
        );
        assert_eq!(bag.state("ai.telemetry.functionId"), Some(Use::Unread));
    }

    #[test]
    fn workflow_name_fills_metadata_unless_that_key_is_already_set() {
        let attrs = [string_attr("traceloop.workflow.name", "answer")];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(
            fields.metadata.get("workflow").map(String::as_str),
            Some("answer")
        );
        assert_eq!(bag.state("traceloop.workflow.name"), Some(Use::Consumed));

        let attrs = [
            string_attr(
                "traceloop.association.properties.workflow",
                "from-association",
            ),
            string_attr("traceloop.workflow.name", "answer"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(
            fields.metadata.get("workflow").map(String::as_str),
            Some("from-association")
        );
        assert_eq!(bag.state("traceloop.workflow.name"), Some(Use::Unread));
    }

    #[test]
    fn metadata_strips_prefixes_and_skips_context_keys() {
        let attrs = [
            string_attr("ai.telemetry.metadata.feature", "later"),
            string_attr("ai.telemetry.metadata.sessionId", "session"),
            string_attr("ai.telemetry.metadata.plan", "pro"),
            string_attr(attr::SESSION_ID, "ours"),
            string_attr("cetra.metadata.feature", "search"),
            int_attr("cetra.metadata.n", 2),
            string_attr(
                "metadata",
                r#"{"feature":"openinference","region":"eu","n":null}"#,
            ),
            string_attr("traceloop.association.properties.user_id", "traceloop-user"),
            string_attr("traceloop.association.properties.topic", "billing"),
            string_attr(attr::EXPERIMENT_ID, "exp-1"),
            string_attr(attr::DATASET_ITEM_ID, "item-1"),
            string_attr("cetra.metadata.cetra.experiment_id", "from-metadata"),
        ];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.session_id.as_deref(), Some("ours"));
        assert_eq!(fields.user_id.as_deref(), Some("traceloop-user"));
        assert_eq!(
            fields.metadata.get("feature").map(String::as_str),
            Some("search")
        );
        assert_eq!(fields.metadata.get("n").map(String::as_str), Some("2"));
        assert_eq!(
            fields.metadata.get("region").map(String::as_str),
            Some("eu")
        );
        assert_eq!(fields.metadata.get("plan").map(String::as_str), Some("pro"));
        assert_eq!(
            fields.metadata.get("topic").map(String::as_str),
            Some("billing")
        );
        assert_eq!(
            fields
                .metadata
                .get(metadata_key::EXPERIMENT_ID)
                .map(String::as_str),
            Some("exp-1")
        );
        assert_eq!(
            fields
                .metadata
                .get(metadata_key::DATASET_ITEM_ID)
                .map(String::as_str),
            Some("item-1")
        );
        assert!(!fields.metadata.contains_key("sessionId"));
        assert!(!fields.metadata.contains_key("user_id"));
        assert_eq!(
            bag.state("ai.telemetry.metadata.sessionId"),
            Some(Use::Unread)
        );
        assert_eq!(
            bag.state("ai.telemetry.metadata.feature"),
            Some(Use::Unread)
        );
        assert_eq!(bag.state("cetra.metadata.feature"), Some(Use::Consumed));
        assert_eq!(bag.state("metadata"), Some(Use::Consumed));
        assert_eq!(
            bag.state("traceloop.association.properties.user_id"),
            Some(Use::Consumed)
        );
        assert_eq!(bag.state(attr::EXPERIMENT_ID), Some(Use::Consumed));
    }

    #[test]
    fn environment_and_release_read_resource_keys() {
        let resource_attrs = [
            string_attr("deployment.environment", "legacy"),
            string_attr("deployment.environment.name", "prod"),
            string_attr("service.version", "1.2.3"),
        ];
        let (fields, _) = filled(&[], &resource_attrs);
        assert_eq!(fields.environment.as_deref(), Some("prod"));
        assert_eq!(fields.release.as_deref(), Some("1.2.3"));

        let (fields, _) = filled(
            &[string_attr(attr::ENVIRONMENT, "staging")],
            &resource_attrs,
        );
        assert_eq!(fields.environment.as_deref(), Some("staging"));

        let (fields, _) = filled(&[], &[string_attr("deployment.environment", "legacy")]);
        assert_eq!(fields.environment.as_deref(), Some("legacy"));

        let attrs = [string_attr("lmnr.span.release", "sha")];
        let (fields, bag) = filled(&attrs, &[]);
        assert_eq!(fields.release.as_deref(), Some("sha"));
        assert_eq!(bag.state("lmnr.span.release"), Some(Use::Consumed));
    }

    #[test]
    fn endpoint_providers_are_rule_12_fixed_points_and_priced_or_listed() {
        // LiteLLM has no chat rows for these; the gateways among them are priced by the
        // publisher's row (D-IN-13).
        const UNPRICED_ENDPOINT_PROVIDERS: &[&str] = &[
            "byteplus",
            "cometapi",
            "doubleword",
            "helicone",
            "huggingface",
            "nvidia_nim",
            "openrouter",
            "portkey",
            "qianfan",
        ];
        let priced: std::collections::BTreeSet<String> = crate::cost::builtin_price_rows()
            .into_iter()
            .filter_map(|row| row.provider)
            .collect();
        for (_, name) in ENDPOINT_PROVIDERS {
            assert_eq!(&canonical_provider(name, false), name);
            assert!(
                priced.contains(*name) || UNPRICED_ENDPOINT_PROVIDERS.contains(name),
                "endpoint provider {name} has no built-in row and is not listed as unpriced"
            );
        }
    }

    #[test]
    fn every_snapshot_provider_is_a_fixed_point_of_rule_12() {
        for row in crate::cost::builtin_price_rows() {
            if let Some(provider) = &row.provider {
                assert_eq!(
                    &canonical_provider(provider, false),
                    provider,
                    "{}",
                    row.model_name
                );
            }
        }
    }

    #[test]
    fn livekit_hosts_in_the_provider_go_through_the_endpoint_table() {
        let attrs = [
            string_attr("gen_ai.provider.name", "api.openai.com"),
            string_attr("gen_ai.request.model", "gpt-4o-mini"),
        ];
        let (fields, bag) = filled_in_scope("livekit-agents", &attrs);
        assert_eq!(fields.provider.as_deref(), Some("openai"));
        assert_eq!(bag.state("gen_ai.provider.name"), Some(Use::Consumed));

        let attrs = [string_attr("gen_ai.provider.name", "openrouter.ai")];
        let (fields, _) = filled_in_scope("livekit-agents", &attrs);
        assert_eq!(fields.provider.as_deref(), Some("openrouter"));

        // An unmapped host leaves the provider empty and is consumed; no later
        // source of the provider is read.
        let attrs = [
            string_attr("gen_ai.provider.name", "127.0.0.1:1234"),
            string_attr("gen_ai.system", "anthropic"),
        ];
        let (fields, bag) = filled_in_scope("livekit-agents", &attrs);
        assert_eq!(fields.provider, None);
        assert_eq!(bag.state("gen_ai.provider.name"), Some(Use::Consumed));
        assert_eq!(bag.state("gen_ai.system"), Some(Use::Unread));

        // A plain provider name is unchanged.
        let attrs = [string_attr("gen_ai.provider.name", "openai")];
        let (fields, _) = filled_in_scope("livekit-agents", &attrs);
        assert_eq!(fields.provider.as_deref(), Some("openai"));

        // Another scope keeps the host string as the provider, as today.
        let attrs = [string_attr("gen_ai.provider.name", "127.0.0.1:1234")];
        let (fields, bag) = filled_in_scope("other.scope", &attrs);
        assert_eq!(fields.provider.as_deref(), Some("127.0.0.1:1234"));
        assert_eq!(bag.state("gen_ai.provider.name"), Some(Use::Consumed));

        // The endpoint table itself is unchanged (D-IN-2).
        assert_eq!(match_endpoint("api.openai.com"), None);
    }

    #[test]
    fn livekit_values_that_are_not_hosts_keep_their_provider() {
        // A dotted provider name that no host rule matches is not a host: the
        // key is left to the normal provider rules.
        for name in ["azure.ai.openai", "gcp.vertex_ai"] {
            let attrs = [string_attr("gen_ai.provider.name", name)];
            let (fields, bag) = filled_in_scope("livekit-agents", &attrs);
            assert_eq!(fields.provider.as_deref(), Some(name), "{name}");
            assert_eq!(
                bag.state("gen_ai.provider.name"),
                Some(Use::Consumed),
                "{name}"
            );
        }

        // An IP address is a host: unmapped, so no provider, and consumed.
        let attrs = [
            string_attr("gen_ai.provider.name", "127.0.0.1"),
            string_attr("gen_ai.system", "anthropic"),
        ];
        let (fields, bag) = filled_in_scope("livekit-agents", &attrs);
        assert_eq!(fields.provider, None);
        assert_eq!(bag.state("gen_ai.provider.name"), Some(Use::Consumed));
        assert_eq!(bag.state("gen_ai.system"), Some(Use::Unread));
    }

    #[test]
    fn pipecat_conversation_id_fills_the_session_on_its_turn_scope_only() {
        let attrs = [string_attr("conversation.id", "conv-1")];
        let (fields, bag) = filled_in_scope("pipecat.turn", &attrs);
        assert_eq!(fields.session_id.as_deref(), Some("conv-1"));
        assert_eq!(bag.state("conversation.id"), Some(Use::Consumed));

        let (fields, bag) = filled_in_scope("pipecat", &attrs);
        assert_eq!(fields.session_id, None);
        assert_eq!(bag.state("conversation.id"), Some(Use::Unread));

        let (fields, bag) = filled_in_scope("other.scope", &attrs);
        assert_eq!(fields.session_id, None);
        assert_eq!(bag.state("conversation.id"), Some(Use::Unread));

        // `gen_ai.conversation.id` still wins when present.
        let attrs = [
            string_attr("gen_ai.conversation.id", "genai-1"),
            string_attr("conversation.id", "conv-1"),
        ];
        let (fields, bag) = filled_in_scope("pipecat.turn", &attrs);
        assert_eq!(fields.session_id.as_deref(), Some("genai-1"));
        assert_eq!(bag.state("conversation.id"), Some(Use::Unread));
    }

    #[test]
    fn not_given_request_params_are_strings_without_a_warning() {
        let attrs = [string_attr("gen_ai.request.top_k", "NOT_GIVEN")];
        let (fields, bag, warnings) = filled_with_warnings(&attrs);
        assert_eq!(
            fields.request_params.as_deref(),
            Some(r#"{"top_k":"NOT_GIVEN"}"#)
        );
        assert_eq!(bag.state("gen_ai.request.top_k"), Some(Use::Consumed));
        assert_eq!(warnings.to_attribute(), None);
    }

    #[test]
    fn promptflow_llm_models_fill_from_the_response_and_the_inputs() {
        // R-PF-2: the response model from `llm.response.model`, the request
        // model from `inputs.model`. `inputs` stays for the input column.
        let attrs = [
            string_attr("span_type", "LLM"),
            string_attr("llm.response.model", "gpt-4o-mini-2024-07-18"),
            string_attr("inputs", r#"{"model":"gpt-4o-mini","messages":[]}"#),
        ];
        let (fields, bag) = filled_in_scope("promptflow", &attrs);
        assert_eq!(fields.request_model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(
            fields.response_model.as_deref(),
            Some("gpt-4o-mini-2024-07-18")
        );
        assert_eq!(bag.state("llm.response.model"), Some(Use::Consumed));
        assert_eq!(bag.state("inputs"), Some(Use::Unread));

        // An embedding span is a model span too.
        let attrs = [
            string_attr("span_type", "Embedding"),
            string_attr("inputs", r#"{"model":"text-embedding-3-small"}"#),
        ];
        let (fields, _) = filled_in_scope("promptflow", &attrs);
        assert_eq!(
            fields.request_model.as_deref(),
            Some("text-embedding-3-small")
        );
    }

    #[test]
    fn promptflow_models_keep_the_genai_model_and_skip_other_span_types() {
        // A model already filled by a higher source is kept.
        let attrs = [
            string_attr("span_type", "LLM"),
            string_attr("gen_ai.request.model", "gpt-4o"),
            string_attr("gen_ai.response.model", "gpt-4o-2024-08-06"),
            string_attr("llm.response.model", "other"),
            string_attr("inputs", r#"{"model":"other"}"#),
        ];
        let (fields, bag) = filled_in_scope("promptflow", &attrs);
        assert_eq!(fields.request_model.as_deref(), Some("gpt-4o"));
        assert_eq!(fields.response_model.as_deref(), Some("gpt-4o-2024-08-06"));
        assert_eq!(bag.state("llm.response.model"), Some(Use::Unread));

        // A `Function` span with a `model` in its inputs gets no model.
        let attrs = [
            string_attr("span_type", "Function"),
            string_attr("inputs", r#"{"model":"gpt-4o-mini"}"#),
        ];
        let (fields, _) = filled_in_scope("promptflow", &attrs);
        assert_eq!(fields.request_model, None);

        // Inputs without `model` leave the request model empty.
        let attrs = [
            string_attr("span_type", "LLM"),
            string_attr("inputs", r#"{"messages":[]}"#),
        ];
        let (fields, _) = filled_in_scope("promptflow", &attrs);
        assert_eq!(fields.request_model, None);

        // The same keys on another scope are unchanged.
        let attrs = [
            string_attr("span_type", "LLM"),
            string_attr("llm.response.model", "m-1"),
            string_attr("inputs", r#"{"model":"m-2"}"#),
        ];
        let (fields, bag) = filled_in_scope("other.scope", &attrs);
        assert_eq!(fields.request_model, None);
        assert_eq!(fields.response_model, None);
        assert_eq!(bag.state("llm.response.model"), Some(Use::Unread));
    }
}
