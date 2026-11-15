//! Realistic span payloads for the benches and the soak (track R M7, R7.2).
//!
//! Everything is a pure function of a trace seed and the corpus: the same seed
//! gives the same bytes. The shapes follow the captured `fixtures/otlp/` cases:
//! messages with parts, tool calls with JSON arguments, system prompts, tool
//! definitions and previews.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use llmobs_core::enums::{Convention, PayloadFormat, SpanKind, SpanStatus};
use llmobs_core::ids::{ProjectId, SpanId, TraceId};
use llmobs_core::span::Span;
use serde_json::{Value, json};

use super::corpus::{Corpus, cut};
use super::rng::{Rng, mix};

pub(crate) const LLM_MIN_BYTES: usize = 10 * 1024;
pub(crate) const LLM_MAX_BYTES: usize = 50 * 1024;
pub(crate) const SMALL_MAX_BYTES: usize = 1024;
pub(crate) const APPS: usize = 16;
const APP_SEED: u64 = 0xA995;
const PREVIEW_CHARS: usize = 512;
const REQUEST_PARAMS: &str = r#"{"max_tokens":1024,"temperature":0.7}"#;
const MODELS: [(&str, &str); 4] = [
    ("openai", "gpt-4o-mini"),
    ("openai", "gpt-4o"),
    ("anthropic", "claude-sonnet-4"),
    ("gcp.gemini", "gemini-2.5-flash"),
];
const RELEASES: [&str; 3] = ["v1.0", "v1.1", "v2.0"];
const TAGS: [&str; 5] = ["beta", "internal", "gpu", "batch", "realtime"];
const STEP_KEYS: [&str; 9] = [
    "http.method",
    "http.route",
    "http.status_code",
    "db.system",
    "db.operation",
    "langgraph.node",
    "langgraph.step",
    "retry.count",
    "cache.hit",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppKind {
    Chat,
    Rag,
    Agent,
}

/// One application of the synthetic workload. `Debug` by hand: sizes and names only.
pub(crate) struct App {
    pub(crate) kind: AppKind,
    /// 1–6 KiB of corpus text joined with "\n\n".
    pub(crate) system: String,
    /// True: the system prompt is a `system` message in `input` (OpenInference,
    /// OpenLLMetry). False: it goes to `system_instructions` (GenAI).
    pub(crate) system_in_input: bool,
    /// `tool_definitions` JSON (spec 02 rule 7), or `None`.
    pub(crate) tools: Option<String>,
    pub(crate) tool_names: Vec<String>,
    pub(crate) provider: &'static str,
    pub(crate) model: &'static str,
    pub(crate) convention: Convention,
}

impl fmt::Debug for App {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("App")
            .field("kind", &self.kind)
            .field("system_bytes", &self.system.len())
            .field("system_in_input", &self.system_in_input)
            .field("tools_bytes", &self.tools.as_ref().map(String::len))
            .field("tool_names", &self.tool_names)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("convention", &self.convention)
            .finish()
    }
}

/// The content fields of one span. Everything else (ids, times, tokens,
/// cost, context) belongs to the caller.
#[derive(Clone, Default)]
pub(crate) struct Content {
    pub(crate) input: Option<String>,
    pub(crate) input_format: Option<PayloadFormat>,
    pub(crate) output: Option<String>,
    pub(crate) output_format: Option<PayloadFormat>,
    pub(crate) input_preview: Option<String>,
    pub(crate) output_preview: Option<String>,
    pub(crate) system_instructions: Option<String>,
    pub(crate) tool_definitions: Option<String>,
    pub(crate) request_params: Option<String>,
    pub(crate) attributes: BTreeMap<String, String>,
}

impl Content {
    /// Sets the ten fields above on `span`, replacing whatever was there.
    pub(crate) fn apply(self, span: &mut Span) {
        span.input = self.input;
        span.input_format = self.input_format;
        span.output = self.output;
        span.output_format = self.output_format;
        span.input_preview = self.input_preview;
        span.output_preview = self.output_preview;
        span.system_instructions = self.system_instructions;
        span.tool_definitions = self.tool_definitions;
        span.request_params = self.request_params;
        span.attributes = self.attributes;
    }

    /// Bytes of every string field, attribute keys and values included.
    pub(crate) fn bytes(&self) -> usize {
        let fields: usize = [
            &self.input,
            &self.output,
            &self.input_preview,
            &self.output_preview,
            &self.system_instructions,
            &self.tool_definitions,
            &self.request_params,
        ]
        .into_iter()
        .flatten()
        .map(String::len)
        .sum();
        let attributes: usize = self
            .attributes
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum();
        fields + attributes
    }
}

impl fmt::Debug for Content {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let size = |field: &Option<String>| field.as_ref().map(String::len);
        f.debug_struct("Content")
            .field("input", &size(&self.input))
            .field("output", &size(&self.output))
            .field("input_preview", &size(&self.input_preview))
            .field("output_preview", &size(&self.output_preview))
            .field("system_instructions", &size(&self.system_instructions))
            .field("tool_definitions", &size(&self.tool_definitions))
            .field("attributes", &self.attributes.len())
            .finish()
    }
}

/// The synthetic workload. `Debug` by hand: app and corpus counts only.
pub(crate) struct Payloads {
    corpus: Corpus,
    apps: Vec<App>,
}

impl fmt::Debug for Payloads {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Payloads")
            .field("apps", &self.apps.len())
            .field("corpus", &self.corpus)
            .finish()
    }
}

/// Index of the app a trace uses: `mix(trace_seed) % APPS`.
pub(crate) fn app_index(trace_seed: u64) -> usize {
    (mix(trace_seed) % APPS as u64) as usize
}

fn build_app(corpus: &Corpus, rng: &mut Rng, a: usize) -> App {
    let texts = corpus.texts();
    let kind = [AppKind::Chat, AppKind::Rag, AppKind::Agent][a % 3];
    let target = 1024 + rng.below(5 * 1024);
    let mut system = String::new();
    while system.len() < target {
        if !system.is_empty() {
            system.push_str("\n\n");
        }
        system.push_str(corpus.text(rng.below(texts)));
    }
    let system = cut(&system, target).to_owned();
    let system_in_input = a.is_multiple_of(2);
    let (tools, tool_names) = if kind == AppKind::Agent || a % 4 == 1 {
        let count = 2 + rng.below(5);
        let names: Vec<String> = (0..count).map(|k| format!("tool_{a}_{k}")).collect();
        let defs: Vec<Value> = names
            .iter()
            .map(|name| {
                let text = corpus.text(rng.below(texts));
                json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": cut(text, 300),
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "query": {"type": "string", "description": cut(text, 120)},
                                "limit": {"type": "integer"}
                            },
                            "required": ["query"]
                        }
                    }
                })
            })
            .collect();
        let tools = serde_json::to_string(&defs).expect("tool definitions serialize");
        (Some(tools), names)
    } else {
        (None, Vec::new())
    };
    let (provider, model) = MODELS[a % MODELS.len()];
    let convention = if system_in_input {
        Convention::Openinference
    } else {
        Convention::Genai
    };
    App {
        kind,
        system,
        system_in_input,
        tools,
        tool_names,
        provider,
        model,
        convention,
    }
}

/// The tool call of an Agent exchange. Texts are kept whole; the cuts are
/// applied when the arguments and the response are built.
struct ToolCall {
    id: String,
    name: String,
    query: String,
    limit: u64,
    /// (title, snippet) pairs.
    results: Vec<(String, String)>,
}

impl ToolCall {
    /// `{"query": cut(query, 120), "limit": n}`, with the query cut to `cap` too.
    fn arguments(&self, cap: usize) -> Value {
        json!({"query": cut(&self.query, cap.min(120)), "limit": self.limit})
    }

    /// `{"results": [{"title": …, "snippet": …}, …]}`, titles cut to 60 and snippets to 200.
    fn response(&self, cap: usize) -> Value {
        let results: Vec<Value> = self
            .results
            .iter()
            .map(|(title, snippet)| {
                json!({
                    "title": cut(title, cap.min(60)),
                    "snippet": cut(snippet, cap.min(200)),
                })
            })
            .collect();
        json!({ "results": results })
    }
}

/// One turn of a trace's conversation.
struct Exchange {
    /// The raw question.
    prompt: String,
    /// The user message: the question, or for RAG the context and the question.
    user_text: String,
    /// RAG: the documents, each cut to 1,536 bytes. Empty otherwise.
    docs: Vec<String>,
    answer: String,
    /// Agent: the tool call between the question and the answer.
    tool: Option<ToolCall>,
}

fn text_part(text: &str) -> Value {
    json!({"type": "text", "content": text})
}

fn message(role: &str, parts: Vec<Value>) -> Value {
    json!({"role": role, "parts": Value::Array(parts)})
}

fn encode(value: &[Value]) -> String {
    serde_json::to_string(value).expect("canonical messages serialize")
}

fn preview(text: &str) -> String {
    text.chars().take(PREVIEW_CHARS).collect()
}

impl Exchange {
    /// The question part: the user message, plus the tool call and its response for Agent exchanges.
    fn question(&self) -> Vec<Value> {
        let mut out = vec![message("user", vec![text_part(&self.user_text)])];
        if let Some(tool) = &self.tool {
            out.push(message(
                "assistant",
                vec![json!({
                    "type": "tool_call",
                    "id": tool.id,
                    "name": tool.name,
                    "arguments": tool.arguments(usize::MAX),
                })],
            ));
            out.push(message(
                "tool",
                vec![json!({
                    "type": "tool_call_response",
                    "id": tool.id,
                    "response": tool.response(usize::MAX),
                })],
            ));
        }
        out
    }

    /// The final assistant message.
    fn answer(&self) -> Value {
        message("assistant", vec![text_part(&self.answer)])
    }

    /// The whole exchange: the question part, then the answer.
    fn whole(&self) -> Vec<Value> {
        let mut out = self.question();
        out.push(self.answer());
        out
    }
}

/// `build(cap)` with the cut limit `cap`, starting at `base`. While the span is
/// at least `SMALL_MAX_BYTES`, the limit drops by a quarter. The plan's 512-byte
/// cuts plus their 512-byte previews overshoot 1 KiB, so the cuts shrink until
/// the span fits.
fn fit(base: usize, build: impl Fn(usize) -> Content) -> Content {
    let mut cap = base;
    loop {
        let content = build(cap);
        if content.bytes() < SMALL_MAX_BYTES || cap <= 16 {
            return content;
        }
        cap = cap * 3 / 4;
    }
}

impl Payloads {
    /// Builds the apps from `APP_SEED`, so every run has the same workload.
    pub(crate) fn new(corpus: Corpus) -> Self {
        let mut rng = Rng::new(APP_SEED);
        let apps = (0..APPS).map(|a| build_app(&corpus, &mut rng, a)).collect();
        Self { corpus, apps }
    }

    pub(crate) fn corpus(&self) -> &Corpus {
        &self.corpus
    }

    pub(crate) fn app(&self, trace_seed: u64) -> &App {
        &self.apps[app_index(trace_seed)]
    }

    /// The first `count` exchanges of a trace. The stream is a prefix: a longer
    /// call gives the same first exchanges.
    fn exchanges(&self, app: &App, trace_seed: u64, count: usize) -> Vec<Exchange> {
        let corpus = &self.corpus;
        let mut rng = Rng::stream(trace_seed, 0xC0);
        (0..count)
            .map(|j| {
                let (prompt, answer) = corpus.turn(rng.below(corpus.turns()));
                let (prompt, answer) = (prompt.to_owned(), answer.to_owned());
                match app.kind {
                    AppKind::Chat => Exchange {
                        user_text: prompt.clone(),
                        prompt,
                        docs: Vec::new(),
                        answer,
                        tool: None,
                    },
                    AppKind::Rag => {
                        let doc_count = 3 + rng.below(4);
                        let docs: Vec<String> = (0..doc_count)
                            .map(|_| {
                                let text = corpus.text(rng.below(corpus.texts()));
                                cut(text, 1536).to_owned()
                            })
                            .collect();
                        let user_text = format!(
                            "Context:\n{}\n\nQuestion: {prompt}",
                            docs.join("\n\n---\n\n")
                        );
                        Exchange {
                            prompt,
                            user_text,
                            docs,
                            answer,
                            tool: None,
                        }
                    }
                    AppKind::Agent => {
                        let name = app.tool_names[rng.below(app.tool_names.len())].clone();
                        let query = corpus.text(rng.below(corpus.texts())).to_owned();
                        let limit = 1 + rng.below(10) as u64;
                        let result_count = 1 + rng.below(3);
                        let results = (0..result_count)
                            .map(|_| {
                                let title = corpus.text(rng.below(corpus.texts())).to_owned();
                                let snippet = corpus.text(rng.below(corpus.texts())).to_owned();
                                (title, snippet)
                            })
                            .collect();
                        Exchange {
                            user_text: prompt.clone(),
                            prompt,
                            docs: Vec::new(),
                            answer,
                            tool: Some(ToolCall {
                                id: format!("call_{j}"),
                                name,
                                query,
                                limit,
                                results,
                            }),
                        }
                    }
                }
            })
            .collect()
    }

    /// The content of LLM call `q`: the exchanges before `q` whole, then the
    /// question part of exchange `q`. Its output is the answer part of exchange `q`.
    fn llm_content(&self, app: &App, exchanges: &[Exchange], q: usize) -> Content {
        let mut messages = Vec::new();
        if app.system_in_input {
            messages.push(message("system", vec![text_part(&app.system)]));
        }
        for exchange in &exchanges[..q] {
            messages.extend(exchange.whole());
        }
        messages.extend(exchanges[q].question());
        let input = encode(&messages);
        let output = encode(&[exchanges[q].answer()]);
        let system_instructions = (!app.system_in_input).then(|| encode(&[text_part(&app.system)]));
        let mut attributes = BTreeMap::new();
        match app.convention {
            Convention::Openinference => {
                attributes.insert("llm.model_name".to_owned(), app.model.to_owned());
                attributes.insert("llm.system".to_owned(), app.provider.to_owned());
                attributes.insert(
                    "llm.invocation_parameters".to_owned(),
                    REQUEST_PARAMS.to_owned(),
                );
                attributes.insert("input.mime_type".to_owned(), "application/json".to_owned());
                attributes.insert("output.mime_type".to_owned(), "application/json".to_owned());
            }
            _ => {
                attributes.insert("gen_ai.operation.name".to_owned(), "chat".to_owned());
            }
        }
        Content {
            input_preview: Some(preview(&exchanges[q].user_text)),
            output_preview: Some(preview(&exchanges[q].answer)),
            input: Some(input),
            input_format: Some(PayloadFormat::Messages),
            output: Some(output),
            output_format: Some(PayloadFormat::Messages),
            system_instructions,
            tool_definitions: app.tools.clone(),
            request_params: Some(REQUEST_PARAMS.to_owned()),
            attributes,
        }
    }

    /// The exchange whose question part ends LLM call 0. Its call-0 content
    /// (input, output and the fixed fields) reaches the size target `T`.
    fn start(&self, app: &App, trace_seed: u64) -> usize {
        let target =
            (LLM_MIN_BYTES as f64 * 5f64.powf(Rng::stream(trace_seed, 0x51).unit())) as usize;
        let mut p = 0;
        loop {
            let exchanges = self.exchanges(app, trace_seed, p + 1);
            let content = self.llm_content(app, &exchanges, p);
            let size = content.input.as_ref().map_or(0, String::len)
                + content.output.as_ref().map_or(0, String::len)
                + content.system_instructions.as_ref().map_or(0, String::len)
                + content.tool_definitions.as_ref().map_or(0, String::len);
            if size >= target {
                return p;
            }
            p += 1;
        }
    }

    /// The `calls` LLM calls of one trace, in order. Each call's input holds the
    /// previous call's whole input and output: a growing conversation.
    pub(crate) fn llm_calls(&self, trace_seed: u64, calls: usize) -> Vec<Content> {
        let app = self.app(trace_seed);
        let p = self.start(app, trace_seed);
        let exchanges = self.exchanges(app, trace_seed, p + calls);
        (0..calls)
            .map(|k| self.llm_content(app, &exchanges, p + k))
            .collect()
    }

    /// The root span: the first question as `text` input, the first answer as
    /// `text` output, both cut (512 bytes, shrunk while the span is over 1 KiB),
    /// and the previews equal the values.
    pub(crate) fn root(&self, trace_seed: u64) -> Content {
        let app = self.app(trace_seed);
        let exchanges = self.exchanges(app, trace_seed, 1);
        let first = &exchanges[0];
        fit(512, |cap| {
            let input = cut(&first.prompt, cap).to_owned();
            let output = cut(&first.answer, cap).to_owned();
            Content {
                input: Some(input.clone()),
                input_format: Some(PayloadFormat::Text),
                output: Some(output.clone()),
                output_format: Some(PayloadFormat::Text),
                input_preview: Some(input),
                output_preview: Some(output),
                ..Default::default()
            }
        })
    }

    /// The tool span between LLM calls `call` and `call + 1` of an agent trace:
    /// the arguments and the response of exchange `p + call + 1` as compact JSON.
    /// Other apps have no tool span and get `Content::default()`.
    pub(crate) fn tool(&self, trace_seed: u64, call: usize) -> Content {
        let app = self.app(trace_seed);
        if app.kind != AppKind::Agent {
            return Content::default();
        }
        let p = self.start(app, trace_seed);
        let exchanges = self.exchanges(app, trace_seed, p + call + 2);
        let Some(tool) = &exchanges[p + call + 1].tool else {
            return Content::default();
        };
        fit(512, |cap| Content {
            input: Some(compact(&tool.arguments(cap))),
            input_format: Some(PayloadFormat::Json),
            output: Some(compact(&tool.response(cap))),
            output_format: Some(PayloadFormat::Json),
            ..Default::default()
        })
    }

    /// The retriever span of a RAG trace: the prompt of exchange `p` as text
    /// input, and its documents as JSON `[{"id", "title", "score"}, …]`.
    /// Other apps get `Content::default()`.
    pub(crate) fn retriever(&self, trace_seed: u64) -> Content {
        let app = self.app(trace_seed);
        if app.kind != AppKind::Rag {
            return Content::default();
        }
        let p = self.start(app, trace_seed);
        let exchanges = self.exchanges(app, trace_seed, p + 1);
        let exchange = &exchanges[p];
        let mut rng = Rng::stream(trace_seed, 0x5E);
        let scores: Vec<f64> = exchange
            .docs
            .iter()
            .map(|_| rng.below(1001) as f64 / 1000.0)
            .collect();
        fit(512, |cap| {
            let prompt = cut(&exchange.prompt, cap).to_owned();
            let docs: Vec<Value> = exchange
                .docs
                .iter()
                .zip(&scores)
                .enumerate()
                .map(|(n, (doc, score))| {
                    json!({
                        "id": format!("doc-{n}"),
                        "title": cut(doc, cap.min(60)),
                        "score": score,
                    })
                })
                .collect();
            let output = compact(&Value::Array(docs));
            let output_preview = cut(&output, cap).to_owned();
            Content {
                input: Some(prompt.clone()),
                input_format: Some(PayloadFormat::Text),
                input_preview: Some(prompt),
                output: Some(output),
                output_format: Some(PayloadFormat::Json),
                output_preview: Some(output_preview),
                ..Default::default()
            }
        })
    }

    /// A framework span (chain step, HTTP call): 3–8 attributes, 200–600 bytes.
    pub(crate) fn step(&self, trace_seed: u64, index: usize) -> Content {
        let mut rng = Rng::stream(trace_seed, 0x5700 + index as u64);
        let count = 3 + rng.below(6);
        let offset = rng.below(STEP_KEYS.len());
        let attributes = (0..count)
            .map(|i| {
                let key_index = (offset + i) % STEP_KEYS.len();
                let value = match key_index {
                    0 => ["GET", "POST", "PUT"][rng.below(3)].to_owned(),
                    1 => format!("/api/v1/traces/{}", rng.below(10_000)),
                    2 => ["200", "201", "404", "500"][rng.below(4)].to_owned(),
                    3 => "sqlite".to_owned(),
                    4 => ["SELECT", "INSERT", "UPDATE"][rng.below(3)].to_owned(),
                    5 => format!("node_{}", rng.below(20)),
                    6 => rng.below(12).to_string(),
                    7 => rng.below(3).to_string(),
                    _ => ["true", "false"][rng.below(2)].to_owned(),
                };
                (STEP_KEYS[key_index].to_owned(), value)
            })
            .collect();
        Content {
            attributes,
            ..Default::default()
        }
    }
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).expect("JSON values serialize")
}

// ---------------------------------------------------------------- whole traces

#[derive(Debug, Clone)]
pub(crate) struct TraceSummary {
    pub(crate) trace_id: TraceId,
    pub(crate) name: String,
    pub(crate) start_time: i64,
    pub(crate) end_time: i64,
    pub(crate) span_count: i64,
    pub(crate) error_count: i64,
    pub(crate) llm_count: i64,
    pub(crate) total_tokens: Option<i64>,
    pub(crate) cost_total: Option<f64>,
    pub(crate) session_id: Option<String>,
    pub(crate) user_id: Option<String>,
}

/// No `Debug`: its spans hold corpus text.
pub(crate) struct GeneratedTrace {
    /// Spans appended when the trace ends.
    pub(crate) first: Vec<Span>,
    /// Spans appended `delay` later (5% of traces), else empty.
    pub(crate) late: Vec<Span>,
    pub(crate) delay: Duration,
    pub(crate) summary: TraceSummary,
}

/// One span of a trace's shape, before its times and context are set.
struct Draft {
    kind: SpanKind,
    name: &'static str,
    content: Content,
    llm: bool,
}

fn span_id(index: usize) -> SpanId {
    SpanId::from_bytes(((index as u64) + 1).to_be_bytes()).expect("span ids are 8 bytes")
}

fn blank_span(
    project: ProjectId,
    trace_id: TraceId,
    index: usize,
    name: &str,
    kind: SpanKind,
    times: (i64, i64),
    ingest: i64,
) -> Span {
    Span {
        project_id: project,
        trace_id,
        span_id: span_id(index),
        parent_span_id: (index > 0).then(|| span_id(0)),
        name: name.to_owned(),
        span_kind: kind,
        start_time: times.0,
        end_time: times.1,
        status: SpanStatus::Unset,
        status_message: None,
        service_name: None,
        session_id: None,
        user_id: None,
        environment: None,
        release: None,
        tags: Vec::new(),
        metadata: BTreeMap::new(),
        provider: None,
        request_model: None,
        response_model: None,
        operation: None,
        input_tokens: None,
        output_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        reasoning_tokens: None,
        total_tokens: None,
        cost_input: None,
        cost_output: None,
        cost_total: None,
        cost_source: None,
        ttft_ns: None,
        request_params: None,
        input: None,
        input_format: None,
        output: None,
        output_format: None,
        input_preview: None,
        output_preview: None,
        system_instructions: None,
        tool_definitions: None,
        attributes: BTreeMap::new(),
        resource_attributes: BTreeMap::new(),
        scope_name: None,
        scope_version: None,
        events: Vec::new(),
        links: Vec::new(),
        media_refs: Vec::new(),
        truncated: Vec::new(),
        conventions: Vec::new(),
        ingest_time: ingest,
        schema_version: 1,
    }
}

/// One trace that ends at `end` (ns). `ingest_time` is `end` on every span;
/// callers that append later overwrite it with the append time.
pub(crate) fn trace(
    payloads: &Payloads,
    project: ProjectId,
    trace_id: TraceId,
    trace_seed: u64,
    end: i64,
) -> GeneratedTrace {
    let app = payloads.app(trace_seed);
    let mut rng = Rng::stream(trace_seed, 0x7A);

    // Shape: the root, then the LLM, tool, retriever and step spans of the app.
    let calls = match app.kind {
        AppKind::Agent => 2 + rng.below(3),
        AppKind::Chat | AppKind::Rag => 1,
    };
    let mut llm_contents = payloads.llm_calls(trace_seed, calls).into_iter();
    let (root_name, root_kind) = match app.kind {
        AppKind::Chat => ("chat", SpanKind::Chain),
        AppKind::Rag => ("rag", SpanKind::Chain),
        AppKind::Agent => ("agent", SpanKind::Agent),
    };
    let mut plan = vec![Draft {
        kind: root_kind,
        name: root_name,
        content: payloads.root(trace_seed),
        llm: false,
    }];
    match app.kind {
        AppKind::Chat => {
            plan.push(llm_draft(llm_contents.next().unwrap_or_default()));
            for _ in 0..6 {
                plan.push(step_draft(payloads, trace_seed, plan.len()));
            }
        }
        AppKind::Rag => {
            plan.push(Draft {
                kind: SpanKind::Retriever,
                name: "retriever",
                content: payloads.retriever(trace_seed),
                llm: false,
            });
            plan.push(llm_draft(llm_contents.next().unwrap_or_default()));
            for _ in 0..6 {
                plan.push(step_draft(payloads, trace_seed, plan.len()));
            }
        }
        AppKind::Agent => {
            for k in 0..calls {
                plan.push(llm_draft(llm_contents.next().unwrap_or_default()));
                if k + 1 < calls {
                    plan.push(Draft {
                        kind: SpanKind::Tool,
                        name: "tool",
                        content: payloads.tool(trace_seed, k),
                        llm: false,
                    });
                }
                for _ in 0..3 {
                    plan.push(step_draft(payloads, trace_seed, plan.len()));
                }
            }
        }
    }
    let n = plan.len();

    // Times: the root covers [end - D, end); span i starts inside it.
    let seconds = rng.lognormal(2.0, 60.0).clamp(0.05, 600.0);
    let duration = (seconds * 1e9) as i64;
    let slots = n as i64 + 1;
    let times: Vec<(i64, i64)> = (0..n)
        .map(|i| {
            if i == 0 {
                (end - duration, end)
            } else {
                let start = end - duration + i as i64 * duration / slots;
                (start, start + duration / (2 * slots))
            }
        })
        .collect();

    // Context, the same on every span.
    let session_id = (rng.unit() < 0.3).then(|| format!("s-{}", rng.below(50_000)));
    let user_id = (rng.unit() < 0.6).then(|| format!("u-{}", rng.below(20_000)));
    let environment = if rng.unit() < 0.9 {
        "production"
    } else {
        "staging"
    };
    let release = RELEASES[rng.below(RELEASES.len())];
    let tag_count = rng.below(4);
    let tags: Vec<String> = (0..tag_count)
        .map(|_| TAGS[rng.below(TAGS.len())].to_owned())
        .collect();
    let service = format!("app-{}", app_index(trace_seed));

    // Status: at most one error span.
    let error_span = (rng.unit() < 0.03).then(|| rng.below(n));

    // Late group: the last third, never the root.
    let late_count = (rng.unit() < 0.05).then(|| (n / 3).max(1));
    let delay = if rng.unit() < 0.8 {
        Duration::from_secs(60 + rng.below(1_741) as u64)
    } else {
        Duration::from_secs(3_600 + rng.below(7_200) as u64)
    };

    let mut spans = Vec::with_capacity(n);
    for (i, draft) in plan.into_iter().enumerate() {
        let mut span = blank_span(project, trace_id, i, draft.name, draft.kind, times[i], end);
        span.service_name = Some(service.clone());
        span.session_id = session_id.clone();
        span.user_id = user_id.clone();
        span.environment = Some(environment.to_owned());
        span.release = Some(release.to_owned());
        span.tags = tags.clone();
        span.status = if error_span == Some(i) {
            SpanStatus::Error
        } else {
            SpanStatus::Ok
        };
        if draft.llm {
            let content = &draft.content;
            let input_bytes = content.input.as_ref().map_or(0, String::len)
                + content.system_instructions.as_ref().map_or(0, String::len)
                + content.tool_definitions.as_ref().map_or(0, String::len);
            let output_bytes = content.output.as_ref().map_or(0, String::len);
            let input_tokens = (input_bytes / 4) as i64;
            let output_tokens = (output_bytes / 4) as i64;
            let total = input_tokens + output_tokens;
            span.provider = Some(app.provider.to_owned());
            span.request_model = Some(app.model.to_owned());
            span.response_model = Some(app.model.to_owned());
            span.input_tokens = Some(input_tokens);
            span.output_tokens = Some(output_tokens);
            span.total_tokens = Some(total);
            span.cost_total = Some(total as f64 / 1_048_576.0);
            span.conventions = vec![app.convention];
        }
        draft.content.apply(&mut span);
        spans.push(span);
    }

    let split = n - late_count.unwrap_or(0);
    let late = spans.split_off(split);
    let first = spans;

    let all = || first.iter().chain(late.iter());
    let llm_count = all().filter(|span| span.span_kind == SpanKind::Llm).count() as i64;
    let summary = TraceSummary {
        trace_id,
        name: first[0].name.clone(),
        start_time: all().map(|span| span.start_time).min().unwrap_or(0),
        end_time: all().map(|span| span.end_time).max().unwrap_or(0),
        span_count: n as i64,
        error_count: all()
            .filter(|span| span.status == SpanStatus::Error)
            .count() as i64,
        llm_count,
        total_tokens: (llm_count > 0).then(|| all().filter_map(|span| span.total_tokens).sum()),
        cost_total: (llm_count > 0).then(|| all().filter_map(|span| span.cost_total).sum()),
        session_id,
        user_id,
    };
    GeneratedTrace {
        first,
        late,
        delay,
        summary,
    }
}

fn llm_draft(content: Content) -> Draft {
    Draft {
        kind: SpanKind::Llm,
        name: "llm",
        content,
        llm: true,
    }
}

fn step_draft(payloads: &Payloads, trace_seed: u64, index: usize) -> Draft {
    Draft {
        kind: SpanKind::Span,
        name: "step",
        content: payloads.step(trace_seed, index),
        llm: false,
    }
}

/// The bytes `Content::bytes` counts for a built span: its payload fields and attributes.
pub(crate) fn span_payload_bytes(span: &Span) -> usize {
    [
        &span.input,
        &span.output,
        &span.input_preview,
        &span.output_preview,
        &span.system_instructions,
        &span.tool_definitions,
        &span.request_params,
    ]
    .into_iter()
    .flatten()
    .map(String::len)
    .sum::<usize>()
        + span
            .attributes
            .iter()
            .map(|(key, value)| key.len() + value.len())
            .sum::<usize>()
}
