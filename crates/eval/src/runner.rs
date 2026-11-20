//! One evaluator run: a check, a judge call, or a webhook, plus its judge trace.
//!
//! [`now_ns`] is the only clock in the crate. A run never returns an error by
//! itself: the score or the failure is [`EvalRun::outcome`], and the trace is
//! always built.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cetra_core::enums::SpanKind;
use cetra_core::ids::TraceId;
use cetra_core::score::ScoreValue;
use cetra_otel::{IngestContext, Normalized, PriceTable};
use serde_json::{Map, Value};

use crate::check::{CheckInput, compile_schema, run_check};
use crate::config::{Check, Evaluator, EvaluatorConfig, JudgeConfig, WebhookConfig};
use crate::egress::{EgressClient, EgressPolicy};
use crate::error::{ConfigError, EvalError, JudgeError, WebhookError};
use crate::judge::{
    Adapter, CompletionCall, EmbedCall, JUDGEMENT_DESCRIPTION, JUDGEMENT_TOOL, JudgeConnection,
    JudgeReply, JudgeUsage, answer_schema, call, embed as call_embedding, find_object,
    parse_answer,
};
use crate::mapping::resolve::Variables;
use crate::secret::Secret;
use crate::template::PromptTemplate;
use crate::trace::{
    CallTraceInput, ChildSpan, EvalOrigin, EvalTarget, TraceInput, build, build_call, call_cost,
    judge_metadata,
};
use crate::webhook::{self, WebhookCall};

/// Runs checks locally and sends judge and webhook calls through one client.
#[derive(Debug)]
pub struct EvalRunner {
    client: EgressClient,
}

/// Where the run sits, and the material the server already decrypted.
#[derive(Debug)]
pub struct RunInput<'a> {
    /// The evaluator to run.
    pub evaluator: &'a Evaluator,
    /// Variables for the check, the template, or the webhook body.
    pub variables: &'a Variables,
    /// Decrypted provider connection. Required for a judge.
    pub connection: Option<&'a JudgeConnection>,
    /// Decrypted signing secret. Required for a webhook.
    pub webhook_secret: Option<&'a Secret>,
    /// Trace and span the score is about.
    pub target: EvalTarget,
    /// Rule, experiment, or the test panel.
    pub origin: EvalOrigin,
    /// Id of the judge trace. The server derives one per attempt (D-I-16).
    pub judge_trace_id: TraceId,
    /// Project, clock, and limits for normalization. The project is `project_id`.
    pub ingest: &'a IngestContext,
    /// Prices for the judge's `llm` span.
    pub prices: &'a PriceTable,
}

/// The score or the failure, and the normalized judge trace either way.
///
/// [`Debug`](fmt::Debug) counts the judge trace's spans instead of printing
/// their payloads.
#[derive(Clone, PartialEq)]
pub struct EvalRun {
    /// `Ok` when a score was produced.
    pub outcome: Result<EvalOutcome, EvalError>,
    /// Spans for this run, including a failed one.
    pub judge_trace: Normalized,
}

/// A score and the judge metadata copied off the normalized trace (D161).
///
/// [`Debug`](fmt::Debug) prints the reason's length, not its text.
#[derive(Clone, PartialEq)]
pub struct EvalOutcome {
    /// The score.
    pub value: ScoreValue,
    /// Explanation or check reason. Absent when the run produced none.
    pub reason: Option<String>,
    /// `judge.*` keys read from the normalized trace.
    pub metadata: BTreeMap<String, String>,
    /// `cost_total` of the `llm` span.
    pub cost_usd: Option<f64>,
    /// The `llm` span had tokens and no price.
    pub unpriced_model: bool,
}

impl fmt::Debug for EvalRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EvalRun")
            .field("outcome", &self.outcome)
            .field("judge_spans", &self.judge_trace.spans.len())
            .field("judge_media", &self.judge_trace.media.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for EvalOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EvalOutcome")
            .field("value", &self.value)
            .field(
                "reason_chars",
                &self.reason.as_ref().map(|reason| reason.chars().count()),
            )
            .field("metadata", &self.metadata)
            .field("cost_usd", &self.cost_usd)
            .field("unpriced_model", &self.unpriced_model)
            .finish()
    }
}

/// Where a call's trace goes and how it is priced (D-IC-17).
#[derive(Debug, Clone, Copy)]
pub struct CallTrace<'a> {
    /// Id of the call trace. The caller derives one per attempt.
    pub trace_id: TraceId,
    /// Name of the root span, e.g. `cetra.issue.summarize`.
    pub root_name: &'a str,
    /// Copied onto the root span as `cetra.metadata.<key>` attributes (ids only, never payload).
    pub metadata: &'a [(&'a str, &'a str)],
    /// Project, clock and limits for normalization.
    pub ingest: &'a IngestContext,
    /// Prices for the child span.
    pub prices: &'a PriceTable,
}

/// One structured completion (D-IC-4).
pub struct CompletionInput<'a> {
    /// Provider connection, already decrypted.
    pub connection: &'a JudgeConnection,
    /// Provider model id.
    pub model: &'a str,
    /// Sent as one user message.
    pub prompt: &'a str,
    /// JSON Schema (draft 2020-12) of the answer object. Must have `"type": "object"`.
    pub schema: &'a Value,
    /// OpenAI `json_schema.name` in native mode, and the forced function in tool mode.
    /// `^[a-zA-Z0-9_-]{1,64}$`.
    pub name: &'a str,
    /// Function description in tool mode.
    pub description: &'a str,
    /// Sampling temperature. Omitted from the request when unset.
    pub temperature: Option<f64>,
    /// Completion cap, including reasoning tokens.
    pub max_output_tokens: u32,
    /// Whole-call timeout.
    pub timeout: Duration,
    /// Where the call trace goes and how it is priced.
    pub trace: CallTrace<'a>,
}

/// The answer or the failure, and the call trace either way.
pub struct CompletionRun {
    /// The answer object, valid against the schema.
    pub outcome: Result<Map<String, Value>, EvalError>,
    /// Token counts, when the provider sent them.
    pub usage: Option<JudgeUsage>,
    /// `cost_total` of the `llm` span.
    pub cost_usd: Option<f64>,
    /// The `llm` span had tokens and no price.
    pub unpriced_model: bool,
    /// Root and `llm` spans, normalized.
    pub trace: Normalized,
}

/// Prints the input's model, name, sizes and timeout, never the prompt or schema.
impl fmt::Debug for CompletionInput<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompletionInput")
            .field("model", &self.model)
            .field("name", &self.name)
            .field("prompt_bytes", &self.prompt.len())
            .field("max_output_tokens", &self.max_output_tokens)
            .field("timeout", &self.timeout)
            .field("trace_id", &self.trace.trace_id)
            .finish_non_exhaustive()
    }
}

/// Prints the outcome's key count or error class, never the answer.
impl fmt::Debug for CompletionRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompletionRun")
            .field(
                "outcome",
                &self
                    .outcome
                    .as_ref()
                    .map(Map::len)
                    .map_err(EvalError::class),
            )
            .field("cost_usd", &self.cost_usd)
            .field("unpriced_model", &self.unpriced_model)
            .field("spans", &self.trace.spans.len())
            .finish_non_exhaustive()
    }
}

/// Largest vector accepted (D-IC-17).
pub const MAX_EMBEDDING_DIMENSIONS: usize = 4096;

/// One embeddings call (D-IC-5).
pub struct EmbeddingInput<'a> {
    /// Provider connection, already decrypted.
    pub connection: &'a JudgeConnection,
    /// Provider embedding model id.
    pub model: &'a str,
    /// The text to embed. Never logged or traced: the trace keeps only its length.
    pub text: &'a str,
    /// Requested size: OpenAI `dimensions`, Gemini `outputDimensionality`. `None` omits it.
    pub dimensions: Option<u32>,
    /// Whole-call timeout.
    pub timeout: Duration,
    /// Where the call trace goes and how it is priced.
    pub trace: CallTrace<'a>,
}

/// The vector or the failure, and the call trace either way.
pub struct EmbeddingRun {
    /// L2-normalized vector, 1 to 4 096 finite values.
    pub outcome: Result<Vec<f32>, EvalError>,
    /// Input tokens the provider reported. `None` when it reported none (Gemini may not).
    pub input_tokens: Option<u64>,
    /// `cost_total` of the `embedding` span. `None` without tokens or a price.
    pub cost_usd: Option<f64>,
    /// The `embedding` span had tokens and no price.
    pub unpriced_model: bool,
    /// Root and `embedding` spans, normalized.
    pub trace: Normalized,
}

/// Prints the model, the text's size, the requested size, the timeout and the trace id, never the text.
impl fmt::Debug for EmbeddingInput<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EmbeddingInput")
            .field("model", &self.model)
            .field("text_bytes", &self.text.len())
            .field("dimensions", &self.dimensions)
            .field("timeout", &self.timeout)
            .field("trace_id", &self.trace.trace_id)
            .finish_non_exhaustive()
    }
}

/// Prints the vector's length or the error class, never the vector.
impl fmt::Debug for EmbeddingRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EmbeddingRun")
            .field(
                "outcome",
                &self
                    .outcome
                    .as_ref()
                    .map(Vec::len)
                    .map_err(EvalError::class),
            )
            .field("input_tokens", &self.input_tokens)
            .field("cost_usd", &self.cost_usd)
            .field("unpriced_model", &self.unpriced_model)
            .field("spans", &self.trace.spans.len())
            .finish_non_exhaustive()
    }
}

/// One completion call, before its trace is built.
struct Attempt {
    outcome: Result<Map<String, Value>, EvalError>,
    /// Set once the provider was called, even when the call failed.
    sent: Option<SentCall>,
}

/// The `llm` child of a completion: the provider's reply, or the failure of the call.
struct SentCall {
    started_ns: i64,
    ended_ns: i64,
    output_text: Option<String>,
    usage: Option<JudgeUsage>,
    response_model: Option<String>,
    error: Option<JudgeError>,
}

struct Finished {
    outcome: Result<EvalOutcome, EvalError>,
    child: Option<OwnedChild>,
}

enum OwnedChild {
    Llm {
        started_ns: i64,
        ended_ns: i64,
        adapter: Adapter,
        request_model: String,
        response_model: Option<String>,
        prompt: String,
        output_text: Option<String>,
        usage: Option<JudgeUsage>,
        error: Option<JudgeError>,
    },
    Webhook {
        started_ns: i64,
        ended_ns: i64,
        host: String,
        status: Option<u16>,
        request_body: Vec<u8>,
        response_body: Option<Value>,
        error: Option<WebhookError>,
    },
}

impl EvalRunner {
    /// Builds the shared client.
    ///
    /// # Errors
    ///
    /// Returns [`EvalError::Egress`] when the TLS client cannot be created.
    pub fn new(policy: EgressPolicy) -> Result<Self, EvalError> {
        Ok(Self {
            client: EgressClient::new(policy)?,
        })
    }

    /// The shared egress client, for other outbound calls that need the same guard
    /// (alert deliveries, D-K-11).
    #[must_use]
    pub fn egress(&self) -> &EgressClient {
        &self.client
    }

    /// Runs `input` once. A failure is [`EvalRun::outcome`]; the trace is always set.
    pub async fn run(&self, input: RunInput<'_>) -> EvalRun {
        let started = now_ns();
        let finished = match &input.evaluator.config {
            EvaluatorConfig::LlmJudge(config) => self.judge(&input, config).await,
            EvaluatorConfig::Check(check) => run_one_check(&input, check),
            EvaluatorConfig::Webhook(config) => self.webhook(&input, config).await,
        };
        let ended = now_ns().max(started);
        tracing::debug!(
            evaluator_id = %input.evaluator.id,
            kind = input.evaluator.config.kind(),
            duration_ns = ended.saturating_sub(started),
            error_class = class_label(&finished.outcome),
            "evaluator run"
        );
        let judge_trace = trace_of(&input, started, ended, &finished);
        let outcome = match finished.outcome {
            Ok(mut outcome) => {
                let (metadata, cost, unpriced) = judge_metadata(&judge_trace, input.judge_trace_id);
                outcome.metadata = metadata;
                outcome.cost_usd = cost;
                outcome.unpriced_model = unpriced;
                Ok(outcome)
            }
            Err(error) => Err(error),
        };
        EvalRun {
            outcome,
            judge_trace,
        }
    }

    /// Runs one structured completion. A failure is [`CompletionRun::outcome`]; the trace is always built.
    pub async fn complete(&self, input: CompletionInput<'_>) -> CompletionRun {
        let started = now_ns();
        let attempt = self.attempt(&input).await;
        let ended = now_ns().max(started);
        let result = match &attempt.outcome {
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        };
        let child = attempt.sent.as_ref().map(|sent| ChildSpan::Llm {
            started_ns: sent.started_ns,
            ended_ns: sent.ended_ns,
            adapter: input.connection.adapter,
            request_model: input.model,
            response_model: sent.response_model.as_deref(),
            prompt: input.prompt,
            output_text: sent.output_text.as_deref(),
            usage: sent.usage.as_ref(),
            error: sent.error.as_ref(),
        });
        let trace = build_call(&CallTraceInput {
            trace: &input.trace,
            started_ns: started,
            ended_ns: ended,
            result,
            child,
        });
        let (cost_usd, unpriced_model) = call_cost(&trace, SpanKind::Llm);
        let usage = attempt.sent.as_ref().and_then(|sent| sent.usage);
        tracing::debug!(
            root_name = input.trace.root_name,
            duration_ns = ended.saturating_sub(started),
            error_class = attempt
                .outcome
                .as_ref()
                .err()
                .map_or("none", |error| error.class().as_str()),
            "completion"
        );
        CompletionRun {
            outcome: attempt.outcome,
            usage,
            cost_usd,
            unpriced_model,
            trace,
        }
    }

    /// Embeds `input.text` once. A failure is [`EmbeddingRun::outcome`]; the trace is always built.
    pub async fn embed(&self, input: EmbeddingInput<'_>) -> EmbeddingRun {
        let started = now_ns();
        let call = EmbedCall {
            model: input.model,
            text: input.text,
            dimensions: input.dimensions,
            timeout: input.timeout,
        };
        let reply = call_embedding(&self.client, input.connection, &call).await;
        let ended = now_ns().max(started);
        let eval_error = reply
            .as_ref()
            .err()
            .map(|error| EvalError::from(error.clone()));
        let result = match &eval_error {
            Some(error) => Err(error),
            None => Ok(()),
        };
        let child = ChildSpan::Embedding {
            started_ns: started,
            ended_ns: ended,
            adapter: input.connection.adapter,
            request_model: input.model,
            response_model: reply
                .as_ref()
                .ok()
                .and_then(|reply| reply.response_model.as_deref()),
            text_chars: input.text.chars().count(),
            input_tokens: reply.as_ref().ok().and_then(|reply| reply.input_tokens),
            dimensions: reply.as_ref().ok().map(|reply| reply.vector.len()),
            error: reply.as_ref().err(),
        };
        let trace = build_call(&CallTraceInput {
            trace: &input.trace,
            started_ns: started,
            ended_ns: ended,
            result,
            child: Some(child),
        });
        let (cost_usd, unpriced_model) = call_cost(&trace, SpanKind::Embedding);
        let input_tokens = reply.as_ref().ok().and_then(|reply| reply.input_tokens);
        let outcome = match reply {
            Ok(reply) => Ok(reply.vector),
            Err(error) => Err(EvalError::from(error)),
        };
        tracing::debug!(
            root_name = input.trace.root_name,
            duration_ns = ended.saturating_sub(started),
            error_class = outcome
                .as_ref()
                .err()
                .map_or("none", |error| error.class().as_str()),
            "embedding"
        );
        EmbeddingRun {
            outcome,
            input_tokens,
            cost_usd,
            unpriced_model,
            trace,
        }
    }

    async fn attempt(&self, input: &CompletionInput<'_>) -> Attempt {
        if !valid_call_name(input.name) {
            return Attempt::unsent(JudgeError::InvalidName);
        }
        let Some(validator) = compile_object_schema(input.schema) else {
            return Attempt::unsent(JudgeError::InvalidSchema);
        };
        let request = CompletionCall {
            model: input.model,
            prompt: input.prompt,
            schema: input.schema,
            schema_name: input.name,
            tool_name: input.name,
            tool_description: input.description,
            temperature: input.temperature,
            max_output_tokens: input.max_output_tokens,
            timeout: input.timeout,
        };
        let started_ns = now_ns();
        let reply = call(&self.client, input.connection, &request).await;
        let ended_ns = now_ns().max(started_ns);
        match reply {
            Err(error) => Attempt {
                outcome: Err(EvalError::from(error.clone())),
                sent: Some(SentCall {
                    started_ns,
                    ended_ns,
                    output_text: None,
                    usage: None,
                    response_model: None,
                    error: Some(error),
                }),
            },
            Ok(JudgeReply {
                answer,
                output_text,
                usage,
                response_model,
            }) => {
                let parsed =
                    find_object(answer).and_then(|object| check_object(object, &validator));
                let error = parsed.as_ref().err().cloned();
                Attempt {
                    outcome: parsed.map_err(EvalError::from),
                    sent: Some(SentCall {
                        started_ns,
                        ended_ns,
                        output_text: Some(output_text),
                        usage: Some(usage),
                        response_model,
                        error,
                    }),
                }
            }
        }
    }

    async fn judge(&self, input: &RunInput<'_>, config: &JudgeConfig) -> Finished {
        let Some(connection) = input.connection else {
            return Finished::failed(ConfigError::Invalid {
                field: "connection",
                reason: "required",
            });
        };
        let template = match PromptTemplate::compile(&config.prompt) {
            Ok(template) => template,
            Err(error) => return Finished::failed(error),
        };
        let prompt = match template.render(input.variables) {
            Ok(prompt) => prompt,
            Err(error) => return Finished::failed(error),
        };
        let schema = answer_schema(&config.output, config.include_reason);
        let request = CompletionCall {
            model: &config.model,
            prompt: &prompt,
            schema: &schema,
            schema_name: "judge_answer",
            tool_name: JUDGEMENT_TOOL,
            tool_description: JUDGEMENT_DESCRIPTION,
            temperature: config.temperature,
            max_output_tokens: config.max_output_tokens,
            timeout: Duration::from_millis(u64::from(config.timeout_ms)),
        };
        let started = now_ns();
        let reply = call(&self.client, connection, &request).await;
        let ended = now_ns().max(started);
        match reply {
            Ok(reply) => {
                let parsed = parse_answer(reply.answer, &config.output, config.include_reason);
                let error = parsed.as_ref().err().cloned();
                let child = OwnedChild::Llm {
                    started_ns: started,
                    ended_ns: ended,
                    adapter: connection.adapter,
                    request_model: config.model.clone(),
                    response_model: reply.response_model,
                    prompt,
                    output_text: Some(reply.output_text),
                    usage: Some(reply.usage),
                    error,
                };
                match parsed {
                    Ok(answer) => Finished {
                        outcome: Ok(outcome(answer.value, answer.explanation)),
                        child: Some(child),
                    },
                    Err(error) => Finished {
                        outcome: Err(error.into()),
                        child: Some(child),
                    },
                }
            }
            Err(error) => Finished {
                outcome: Err(EvalError::from(error.clone())),
                child: Some(OwnedChild::Llm {
                    started_ns: started,
                    ended_ns: ended,
                    adapter: connection.adapter,
                    request_model: config.model.clone(),
                    response_model: None,
                    prompt,
                    output_text: None,
                    usage: None,
                    error: Some(error),
                }),
            },
        }
    }

    async fn webhook(&self, input: &RunInput<'_>, config: &WebhookConfig) -> Finished {
        let Some(secret) = input.webhook_secret else {
            return Finished::failed(ConfigError::Invalid {
                field: "secret",
                reason: "required",
            });
        };
        let started = now_ns();
        let invocation = WebhookCall {
            config,
            secret,
            evaluator_id: input.evaluator.id,
            evaluator_name: &input.evaluator.name,
            project_id: input.ingest.project_id,
            trace_id: input.target.trace_id,
            span_id: input.target.span_id,
            variables: input.variables,
            output: &input.evaluator.output,
            now_unix_s: started.div_euclid(1_000_000_000),
        };
        let sent = webhook::exchange(&self.client, &invocation).await;
        let ended = now_ns().max(started);
        let webhook::WebhookExchange {
            host,
            status,
            request_body,
            response_body,
            result,
        } = sent;
        let (outcome, error) = match result {
            Ok(reply) => (Ok(outcome(reply.value, reply.reason)), None),
            Err(error) => (Err(EvalError::from(error.clone())), Some(error)),
        };
        let child = host.map(|host| OwnedChild::Webhook {
            started_ns: started,
            ended_ns: ended,
            host,
            status,
            request_body,
            response_body,
            error,
        });
        Finished { outcome, child }
    }
}

impl Finished {
    fn failed(error: impl Into<EvalError>) -> Self {
        Self {
            outcome: Err(error.into()),
            child: None,
        }
    }
}

impl OwnedChild {
    fn view(&self) -> ChildSpan<'_> {
        match self {
            Self::Llm {
                started_ns,
                ended_ns,
                adapter,
                request_model,
                response_model,
                prompt,
                output_text,
                usage,
                error,
            } => ChildSpan::Llm {
                started_ns: *started_ns,
                ended_ns: *ended_ns,
                adapter: *adapter,
                request_model,
                response_model: response_model.as_deref(),
                prompt,
                output_text: output_text.as_deref(),
                usage: usage.as_ref(),
                error: error.as_ref(),
            },
            Self::Webhook {
                started_ns,
                ended_ns,
                host,
                status,
                request_body,
                response_body,
                error,
            } => ChildSpan::Webhook {
                started_ns: *started_ns,
                ended_ns: *ended_ns,
                host,
                status: *status,
                request_body,
                response_body: response_body.as_ref(),
                error: error.as_ref(),
            },
        }
    }
}

fn run_one_check(input: &RunInput<'_>, check: &Check) -> Finished {
    let check_input = CheckInput {
        output: input.variables.get("output"),
        expected: input.variables.get("expected"),
    };
    match run_check(check, &check_input) {
        Ok(result) => Finished {
            outcome: Ok(outcome(result.value, result.reason)),
            child: None,
        },
        Err(error) => Finished::failed(error),
    }
}

impl Attempt {
    /// A call that failed before the provider was called. Its trace has no child.
    fn unsent(error: JudgeError) -> Self {
        Self {
            outcome: Err(EvalError::from(error)),
            sent: None,
        }
    }
}

/// `^[a-zA-Z0-9_-]{1,64}$`, checked byte by byte.
fn valid_call_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// The compiled schema, when it is an object schema that compiles.
fn compile_object_schema(schema: &Value) -> Option<jsonschema::Validator> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return None;
    }
    compile_schema(schema).ok()
}

/// The answer object, when the schema accepts it. The schema's error text is not kept.
fn check_object(
    object: Map<String, Value>,
    validator: &jsonschema::Validator,
) -> Result<Map<String, Value>, JudgeError> {
    let instance = Value::Object(object);
    if !validator.is_valid(&instance) {
        return Err(JudgeError::SchemaMismatch);
    }
    match instance {
        Value::Object(object) => Ok(object),
        _ => Err(JudgeError::NotAnObject),
    }
}

fn outcome(value: ScoreValue, reason: Option<String>) -> EvalOutcome {
    EvalOutcome {
        value,
        reason,
        metadata: BTreeMap::new(),
        cost_usd: None,
        unpriced_model: false,
    }
}

fn trace_of(input: &RunInput<'_>, started: i64, ended: i64, finished: &Finished) -> Normalized {
    let result = match &finished.outcome {
        Ok(outcome) => Ok((&outcome.value, outcome.reason.as_deref())),
        Err(error) => Err(error),
    };
    build(
        &TraceInput {
            trace_id: input.judge_trace_id,
            evaluator: input.evaluator,
            target: &input.target,
            origin: &input.origin,
            started_ns: started,
            ended_ns: ended,
            variables: input.variables,
            result,
            child: finished.child.as_ref().map(OwnedChild::view),
        },
        input.ingest,
        input.prices,
    )
}

fn class_label(outcome: &Result<EvalOutcome, EvalError>) -> &'static str {
    match outcome {
        Ok(_) => "none",
        Err(error) => error.class().as_str(),
    }
}

/// Nanoseconds since the Unix epoch. A clock before the epoch is negative.
fn now_ns() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
        Err(earlier) => -i64::try_from(earlier.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}
