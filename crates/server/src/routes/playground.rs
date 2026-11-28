//! Playground runs (spec 09 § Playground, D-V-20 to D-V-26, D-V-33 to D-V-37): one model run
//! through the org's LLM connection, streamed as `text/event-stream`, and traced into the reserved
//! environment `cetra-playground` (not billed).
//!
//! The handler validates and admits the run, then spawns one task that owns the admission and
//! the connection's slot. The task streams the provider's events, commits the run's trace after
//! the stream ends (also when the client left), and then sends the last event if the client is
//! still there.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::http::Method;
use axum::response::Response;
use axum::response::sse::Event;
use axum::routing::post;
use cetra_core::ids::TraceId;
use cetra_core::message::{Message, Part, Role};
use cetra_eval::plain_template::PlainTemplateError;
use cetra_eval::playground::{
    PlaygroundEntry, PlaygroundTrace, RenderError, build_playground_trace, render_messages,
};
use cetra_eval::{
    CHAT_MAX_TOKENS_DEFAULT, CHAT_MAX_TOKENS_MAX, CHAT_TIMEOUT_MAX, ChatDone, ChatError, ChatEvent,
    ChatParams, ChatRequest, ChatStream, ErrorClass, JudgeConnection, ResponseFormat, TokenUsage,
    ToolCall, ToolChoice, ToolDefinition,
};
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use uuid::Uuid;

use crate::analytics::actor::Actor;
use crate::analytics::catalog::Event as CatalogEvent;
use crate::analytics::record;
use crate::api::common::{FieldError, TextPosition};
use crate::api::playground::{
    PlaygroundParams, PlaygroundRunRequest, PlaygroundSource, PlaygroundTool,
};
use crate::auth::limits::PlaygroundGuard;
use crate::auth::principal::{self, Principal};
use crate::auth::roles::ProjectPerm;
use crate::eval::execute::commit_judge_trace;
use crate::eval::interactive::{self, InteractivePermit};
use crate::http::AppState;
use crate::http::access::{Access, ProjectAccess, RouteDef};
use crate::http::error::{self, ApiError, field_error};
use crate::http::restore::refuse_outbound;
use crate::ingest::CommitGuard;
use crate::live::{StreamTiming, shut};
use crate::meta::connections;
use crate::meta::projects::ProjectRow;
use crate::routes::streams::{json_event, ping_event, send, sse_response};

/// Largest request body, in bytes (spec 09 § Playground).
const BODY_LIMIT: usize = 2 * 1024 * 1024;

/// Events a run may have queued for its client before it waits (as the live streams).
const CLIENT_BUFFER: usize = 16;

/// How long a run waits for its connection's slot and rate token (D-V-35).
pub(crate) const INTERACTIVE_WAIT: Duration = Duration::from_secs(10);

/// How long the provider may stay silent before the run fails: its connect or head wait, and the
/// gap between two of its chunks (spec 09 § Playground: 60 s). Pings do not count as provider
/// output.
#[cfg(not(test))]
pub(crate) const RUN_IDLE: Duration = cetra_eval::CHAT_IDLE_TIMEOUT;

/// Short in tests, so a silent provider is tested in seconds.
#[cfg(test)]
pub(crate) const RUN_IDLE: Duration = Duration::from_secs(5);

/// The tool that carries the answer when a response format is set (D-V-25).
const SUBMIT_ANSWER: &str = "submit_answer";

/// The description of the forced answer tool.
const SUBMIT_ANSWER_DESCRIPTION: &str = "Submit the answer.";

/// Largest number of messages after placeholders are expanded (D-V-37).
const MAX_MESSAGES: usize = 200;

/// The error text of a run the client or a shutdown ended (D-V-37).
const CANCELLED: &str = "cancelled";

/// The playground route.
pub(crate) fn routes() -> Vec<RouteDef> {
    vec![RouteDef {
        method: Method::POST,
        path: "/api/v1/projects/{project}/playground/runs",
        access: Access::Project {
            perm: ProjectPerm::RunPlayground,
            session_only: false,
        },
        documented: true,
        handler: post(run_playground).layer(DefaultBodyLimit::max(BODY_LIMIT)),
    }]
}

/// Admits one run and answers its event stream (D-V-20). Every refusal before the stream is a
/// problem answer.
async fn run_playground(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Extension(access): Extension<ProjectAccess>,
    error::Json(body): error::Json<PlaygroundRunRequest>,
) -> Result<Response, ApiError> {
    refuse_outbound(&state, "playground run")?;
    let user_id = principal::session_user(&principal)?.id;
    let entries = parse_entries(&body.messages)?;
    let placeholders = parse_placeholders(&body.placeholders)?;
    check_tool_names(&body.tools)?;
    let choice = body
        .tool_choice
        .as_deref()
        .map(|choice| tool_choice(choice, &body.tools))
        .transpose()?;
    check_reserved(&body)?;
    check_model(&body.model)?;
    check_source(body.source.as_ref())?;
    let max_tokens = check_max_tokens(body.params.max_tokens)?;
    let messages = render_messages(
        &entries,
        &body.variables,
        &placeholders,
        body.render_variables,
    )
    .map_err(render_error)?;
    let request = ChatRequest {
        model: body.model.clone(),
        messages,
        tools: body.tools.iter().map(tool_definition).collect(),
        tool_choice: choice,
        response_format: body.response_format.as_ref().map(|format| ResponseFormat {
            name: format.name.clone(),
            schema: format.schema.clone(),
            strict: format.strict,
            tool_name: SUBMIT_ANSWER.to_owned(),
            tool_description: SUBMIT_ANSWER_DESCRIPTION.to_owned(),
        }),
        params: chat_params(&body.params),
        max_tokens,
        timeout: CHAT_TIMEOUT_MAX,
        idle_timeout: RUN_IDLE,
    };
    cetra_eval::check_chat_request(&request).map_err(check_error)?;
    let row = connections::get_connection(
        state.meta.reader(),
        access.project.org_id,
        body.connection_id,
    )
    .await?
    .ok_or_else(|| field_error("connection_id", "connection not found in this organization"))?;
    // D-V-36: the connection is used whatever its status; it is never marked failing here.
    let connection = state
        .connections
        .get(&state.secrets, &row)
        .map_err(internal)?;
    let now = state.clock.now_ns();
    // Admission (the org's and the user's limits) comes before the connection's slot, so a run
    // that answers 429 for a busy connection has already spent its place in the user's minute
    // (spec 09 § Playground).
    let guard = state
        .limits
        .playground
        .admit(user_id, access.project.org_id, now)
        .map_err(|retry| ApiError::RateLimited {
            retry_after_s: retry.0.as_secs().max(1),
        })?;
    let permit = interactive::acquire(&state, &row, INTERACTIVE_WAIT)
        .await
        .map_err(|_busy| ApiError::RateLimited { retry_after_s: 1 })?;
    let trace_id = new_trace_id()?;
    let (source, prompt) = source_of(body.source.as_ref());
    let (tx, rx) = mpsc::channel(CLIENT_BUFFER);
    // Held until the run task ends, so shutdown waits for the run's trace commit (D-G-38).
    let commits = state.ingest.hold_commits();
    tokio::spawn(run_task(RunTask {
        state: state.clone(),
        project: access.project.clone(),
        run_by: user_id,
        connection_id: row.id,
        connection,
        request,
        trace_id,
        source,
        prompt,
        tx,
        _guard: guard,
        _permit: permit,
        _commits: commits,
    }));
    record(
        &state,
        &CatalogEvent::PlaygroundRun,
        &Actor::project(&principal, &access),
    );
    Ok(sse_response(rx))
}

/// What the run task needs. The task holds the admission and the slot until it ends.
struct RunTask {
    state: AppState,
    project: ProjectRow,
    run_by: Uuid,
    connection_id: Uuid,
    connection: Arc<JudgeConnection>,
    request: ChatRequest,
    trace_id: TraceId,
    /// `prompt:<name>@<version>` or `span:<trace>:<span>`.
    source: Option<String>,
    /// The source prompt's name and version.
    prompt: Option<(String, Option<i64>)>,
    tx: mpsc::Sender<Event>,
    /// The org's and the user's in-flight slots and the rate token.
    _guard: PlaygroundGuard,
    /// The connection's concurrency slot.
    _permit: InteractivePermit,
    /// The ingest commits guard: shutdown waits for the run's trace commit until the task ends.
    _commits: CommitGuard,
}

/// What the stream read gathered.
#[derive(Default)]
struct Run {
    /// Answer text, including a submitted answer.
    text: String,
    /// Tool calls the model made, other than the answer.
    tool_calls: Vec<ToolCall>,
    /// The provider's last event, when the stream reached it.
    done: Option<ChatDone>,
    /// The forced answer tool's call was streamed as text (tool mode).
    answered: bool,
    /// The error that ended the run.
    error: Option<ChatError>,
    /// Time from the start of the run to the first text or tool call.
    ttft: Option<Duration>,
    /// The client left, or the server is shutting down.
    cancelled: bool,
}

impl Run {
    fn first_token(&mut self, started: Instant) {
        if self.ttft.is_none() {
            self.ttft = Some(started.elapsed());
        }
    }
}

/// The SSE sender of one run, numbering its events.
struct Emitter {
    tx: mpsc::Sender<Event>,
    next_id: u64,
}

impl Emitter {
    /// Sends a named JSON event. `false` when the client is gone or did not make room in time.
    async fn json(&mut self, name: &str, value: &Value) -> bool {
        let id = self.next_id;
        self.next_id += 1;
        match json_event(name, id, value) {
            Some(event) => send(&self.tx, event, StreamTiming::DEFAULT.send).await,
            None => true,
        }
    }

    async fn ping(&self) -> bool {
        send(&self.tx, ping_event(), StreamTiming::DEFAULT.send).await
    }
}

/// Runs the request and sends its events, then commits the run's trace (D-V-22, D-V-37).
async fn run_task(task: RunTask) {
    let RunTask {
        state,
        project,
        run_by,
        connection_id,
        connection,
        request,
        trace_id,
        source,
        prompt,
        tx,
        _guard,
        _permit,
        _commits,
    } = task;
    let started_ns = state.clock.now_ns();
    let started = Instant::now();
    let mut closed = state.live.closed();
    let mut emitter = Emitter { tx, next_id: 0 };
    let mut run = Run::default();
    if emitter
        .json("start", &json!({"trace_id": trace_id.to_hex()}))
        .await
    {
        // The client or a shutdown may leave before the provider answers its head (D-V-37): the
        // provider call is dropped then, and the run is cancelled.
        let opened = tokio::select! {
            biased;
            () = shut(&mut closed) => None,
            () = emitter.tx.closed() => None,
            result = cetra_eval::stream_chat(state.eval_runner.egress(), connection.as_ref(), &request) => Some(result),
        };
        match opened {
            None => run.cancelled = true,
            Some(Ok(stream)) => {
                let submit_answer = request.response_format.is_some();
                read_stream(
                    stream,
                    &mut emitter,
                    &mut run,
                    &mut closed,
                    started,
                    submit_answer,
                )
                .await;
            }
            Some(Err(error)) => run.error = Some(error),
        }
    } else {
        run.cancelled = true;
    }

    let elapsed = started.elapsed();
    let latency_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    let ended_ns = started_ns.saturating_add(i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX));
    let error_text = if run.cancelled {
        Some(CANCELLED.to_owned())
    } else {
        run.error.as_ref().map(ToString::to_string)
    };
    // In tool mode the provider reports a tool call for the answer. When the answer was streamed
    // as text and no other tool call exists, the run stopped normally (D-V-25).
    let answered_only = run.answered && run.tool_calls.is_empty();
    let finish = run
        .done
        .as_ref()
        .and_then(|done| done.finish_reason.as_ref())
        .map(|reason| {
            if answered_only && matches!(reason.as_str(), "tool_call" | "tool_calls") {
                "stop"
            } else {
                reason.as_str()
            }
        });
    let usage = run.done.as_ref().and_then(|done| done.usage);
    let response_model = run
        .done
        .as_ref()
        .and_then(|done| done.response_model.as_deref());
    let output = assistant_message(&run, finish);
    let ttft = run.ttft;
    let trace = PlaygroundTrace {
        trace_id,
        started_ns,
        ended_ns,
        adapter: connection.adapter,
        request: &request,
        response_model,
        output: output.as_ref(),
        usage: usage.as_ref(),
        finish_reason: finish,
        ttft_ns: ttft.map(|wait| i64::try_from(wait.as_nanos()).unwrap_or(i64::MAX)),
        error: error_text.as_deref(),
        run_by,
        source,
        prompt: prompt
            .as_ref()
            .map(|(name, version)| (name.as_str(), *version)),
    };
    let ingest = state.ingest.context(&project, None, ended_ns);
    let prices = state
        .ingest
        .prices()
        .for_project(project.org_id, project.id);
    let normalized = build_playground_trace(&trace, &ingest, &prices);
    let cost_usd = normalized.spans.first().and_then(|span| span.cost_total);
    // The trace is committed whether or not the client is still there (D-V-37). A failure is
    // logged with ids only, and the run's last event is still sent.
    if commit_judge_trace(&state, &project, ended_ns, normalized)
        .await
        .is_err()
    {
        tracing::warn!(
            project_id = %project.id,
            trace_id = %trace_id.to_hex(),
            "could not commit the playground trace"
        );
    }

    let outcome = if run.cancelled {
        if !emitter.tx.is_closed() {
            emitter
                .json(
                    "error",
                    &json!({"class": "retryable", "message": CANCELLED}),
                )
                .await;
        }
        "cancelled"
    } else if let Some(error) = &run.error {
        emitter
            .json(
                "error",
                &json!({"class": class_name(error.class()), "message": error.to_string()}),
            )
            .await;
        "error"
    } else {
        emitter
            .json(
                "done",
                &json!({
                    "finish_reason": finish,
                    "usage": usage.as_ref().map(usage_json),
                    "cost_usd": cost_usd,
                    "latency_ms": latency_ms,
                    "ttft_ms": ttft.map(|wait| u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)),
                    "output": output,
                }),
            )
            .await;
        "done"
    };
    tracing::info!(
        project_id = %project.id,
        connection_id = %connection_id,
        user_id = %run_by,
        trace_id = %trace_id.to_hex(),
        outcome,
        latency_ms,
        "playground run"
    );
}

/// Reads the provider's next event and gives the stream back with it. The read is a future that
/// owns the stream, so the select loop in [`read_stream`] can hold it across ping ticks.
async fn pull(mut stream: ChatStream) -> (ChatStream, Result<Option<ChatEvent>, ChatError>) {
    let next = stream.next().await;
    (stream, next)
}

/// Forwards the provider's events to the client until the stream ends, fails, or the client or
/// the server goes away. Dropping `stream` closes the provider's request.
///
/// The provider read is held across the ping ticks. Re-creating it on every ping would restart
/// the provider's idle timeout (inside the read) each time, so a silent provider would never
/// time out.
async fn read_stream(
    stream: ChatStream,
    emitter: &mut Emitter,
    run: &mut Run,
    closed: &mut watch::Receiver<bool>,
    started: Instant,
    submit_answer: bool,
) {
    let mut ping = interval_at(
        Instant::now() + StreamTiming::DEFAULT.ping,
        StreamTiming::DEFAULT.ping,
    );
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut pending = Box::pin(pull(stream));
    loop {
        tokio::select! {
            biased;
            () = shut(&mut *closed) => {
                run.cancelled = true;
                return;
            }
            () = emitter.tx.closed() => {
                run.cancelled = true;
                return;
            }
            _ = ping.tick() => {
                if !emitter.ping().await {
                    run.cancelled = true;
                    return;
                }
            }
            (stream, next) = &mut pending => {
                match next {
                    Ok(Some(ChatEvent::Text(delta))) => {
                        run.first_token(started);
                        run.text.push_str(&delta);
                        if !emitter.json("text", &json!({"delta": delta})).await {
                            run.cancelled = true;
                            return;
                        }
                    }
                    Ok(Some(ChatEvent::ToolCall(call))) => {
                        run.first_token(started);
                        if submit_answer && call.name == SUBMIT_ANSWER {
                            let delta = call.arguments.to_string();
                            run.answered = true;
                            run.text.push_str(&delta);
                            if !emitter.json("text", &json!({"delta": delta})).await {
                                run.cancelled = true;
                                return;
                            }
                        } else {
                            let event = json!({
                                "id": call.id,
                                "name": call.name,
                                "arguments": call.arguments,
                            });
                            if !emitter.json("tool_call", &event).await {
                                run.cancelled = true;
                                return;
                            }
                            run.tool_calls.push(call);
                        }
                    }
                    Ok(Some(ChatEvent::Done(done))) => {
                        run.done = Some(done);
                        return;
                    }
                    Ok(None) => return,
                    Err(error) => {
                        run.error = Some(error);
                        return;
                    }
                }
                pending = Box::pin(pull(stream));
            }
        }
    }
}

/// The assistant message of the run: its text part, then its tool-call parts. `None` when both
/// are empty (D-V-22).
fn assistant_message(run: &Run, finish: Option<&str>) -> Option<Message> {
    if run.text.is_empty() && run.tool_calls.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(run.tool_calls.len() + 1);
    if !run.text.is_empty() {
        parts.push(Part::Text {
            content: run.text.clone(),
            extra: Map::new(),
        });
    }
    parts.extend(run.tool_calls.iter().map(|call| Part::ToolCall {
        id: Some(call.id.clone()),
        name: call.name.clone(),
        arguments: Some(call.arguments.clone()),
        extra: Map::new(),
    }));
    Some(Message {
        role: Role::Assistant,
        name: None,
        parts,
        finish_reason: finish.map(str::to_owned),
        extra: Map::new(),
    })
}

fn usage_json(usage: &TokenUsage) -> Value {
    json!({
        "input": usage.input,
        "output": usage.output,
        "cache_read": usage.cache_read,
        "cache_write": usage.cache_write,
        "reasoning": usage.reasoning,
    })
}

/// The error class names of the stream's last event (D-V-36, D160).
fn class_name(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Retryable => "retryable",
        ErrorClass::Pause => "pause",
        ErrorClass::Permanent => "permanent",
    }
}

/// Parses each request entry as a canonical message or a placeholder (D-V-37).
fn parse_entries(values: &[Value]) -> Result<Vec<PlaygroundEntry>, ApiError> {
    if values.is_empty() {
        return Err(field_error("messages", "must not be empty"));
    }
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            serde_json::from_value::<PlaygroundEntry>(value.clone()).map_err(|_| {
                field_error(
                    &format!("messages[{index}]"),
                    "must be a message or a placeholder",
                )
            })
        })
        .collect()
}

/// Parses the placeholders' messages (D-V-37).
fn parse_placeholders(
    values: &BTreeMap<String, Vec<Value>>,
) -> Result<BTreeMap<String, Vec<Message>>, ApiError> {
    let mut parsed = BTreeMap::new();
    for (name, messages) in values {
        let mut list = Vec::with_capacity(messages.len());
        for (index, value) in messages.iter().enumerate() {
            let message = serde_json::from_value::<Message>(value.clone()).map_err(|_| {
                field_error(
                    &format!("placeholders.{name}[{index}]"),
                    "must be a message",
                )
            })?;
            list.push(message);
        }
        parsed.insert(name.clone(), list);
    }
    Ok(parsed)
}

/// `[A-Za-z0-9_-]`, 1 to 64 bytes.
fn valid_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Every tool name is valid and no name repeats (D-V-25).
fn check_tool_names(tools: &[PlaygroundTool]) -> Result<(), ApiError> {
    for (index, tool) in tools.iter().enumerate() {
        if !valid_tool_name(&tool.name) {
            return Err(field_error(
                &format!("tools[{index}].name"),
                "must be 1 to 64 letters, digits, _ or -",
            ));
        }
    }
    let mut seen = HashSet::new();
    for (index, tool) in tools.iter().enumerate() {
        if !seen.insert(tool.name.as_str()) {
            return Err(field_error(
                &format!("tools[{index}].name"),
                "must repeat no tool name",
            ));
        }
    }
    Ok(())
}

/// `auto`, `none`, `required`, or the name of one of the request's tools.
fn tool_choice(choice: &str, tools: &[PlaygroundTool]) -> Result<ToolChoice, ApiError> {
    match choice {
        "auto" => Ok(ToolChoice::Auto),
        "none" => Ok(ToolChoice::None),
        "required" => Ok(ToolChoice::Required),
        name if tools.iter().any(|tool| tool.name == name) => Ok(ToolChoice::Tool(name.to_owned())),
        _ => Err(field_error(
            "tool_choice",
            "must be auto, none, required or the name of a tool",
        )),
    }
}

/// With a response format, no tool may take the answer tool's name.
fn check_reserved(body: &PlaygroundRunRequest) -> Result<(), ApiError> {
    if body.response_format.is_none() {
        return Ok(());
    }
    match body
        .tools
        .iter()
        .position(|tool| tool.name == SUBMIT_ANSWER)
    {
        Some(index) => Err(field_error(
            &format!("tools[{index}].name"),
            "submit_answer is reserved",
        )),
        None => Ok(()),
    }
}

/// 1 to 256 bytes with no whitespace, control character, or `/ \ % ? #`.
fn check_model(model: &str) -> Result<(), ApiError> {
    let valid = !model.is_empty()
        && model.len() <= 256
        && !model.chars().any(|ch| {
            ch.is_whitespace() || ch.is_control() || matches!(ch, '/' | '\\' | '%' | '?' | '#')
        });
    if valid {
        Ok(())
    } else {
        Err(field_error(
            "model",
            "must be 1 to 256 bytes with no whitespace, control character, / \\ % ? or #",
        ))
    }
}

/// The completion cap: the request's, or the default, from 1 to 65 536 (D-V-24).
fn check_max_tokens(requested: Option<u32>) -> Result<u32, ApiError> {
    let max = requested.unwrap_or(CHAT_MAX_TOKENS_DEFAULT);
    if (1..=CHAT_MAX_TOKENS_MAX).contains(&max) {
        Ok(max)
    } else {
        Err(field_error("params.max_tokens", "must be from 1 to 65536"))
    }
}

fn tool_definition(tool: &PlaygroundTool) -> ToolDefinition {
    ToolDefinition {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: tool.parameters.clone(),
    }
}

fn chat_params(params: &PlaygroundParams) -> ChatParams {
    ChatParams {
        temperature: params.temperature,
        top_p: params.top_p,
        stop: params.stop.clone(),
        presence_penalty: params.presence_penalty,
        frequency_penalty: params.frequency_penalty,
        seed: params.seed,
        reasoning_effort: params.reasoning_effort.clone(),
    }
}

/// A rendering failure as a field error. The field names the entry and the part, never the text.
fn render_error(error: RenderError) -> ApiError {
    match error {
        RenderError::Template {
            index,
            part,
            source,
        } => {
            let position = match &source {
                PlainTemplateError::Invalid { at, .. } => Some(TextPosition {
                    line: at.line,
                    column: at.column,
                }),
                _ => None,
            };
            ApiError::BadRequest {
                detail: None,
                errors: vec![FieldError {
                    field: format!("messages[{index}].parts[{part}]"),
                    message: source.to_string(),
                    position,
                }],
            }
        }
        RenderError::MissingPlaceholder { .. } => field_error(
            "placeholders",
            "a placeholder entry names a placeholder that is not given",
        ),
        RenderError::TooMany => field_error(
            "messages",
            &format!("must be at most {MAX_MESSAGES} messages after placeholders"),
        ),
        RenderError::TooLarge => field_error("messages", &RenderError::TooLarge.to_string()),
    }
}

/// The source names a prompt version: its name follows the prompt name rule (D-V-2), and its
/// version is at least 1 (D-V-8).
fn check_source(source: Option<&PlaygroundSource>) -> Result<(), ApiError> {
    if let Some(PlaygroundSource::Prompt { name, version }) = source {
        crate::prompts::check_name(name)
            .map_err(|error| field_error("source.name", &error.message))?;
        if *version < 1 {
            return Err(field_error("source.version", "must be at least 1"));
        }
    }
    Ok(())
}

/// A failed check of the built request. Its text names no payload.
fn check_error(error: ChatError) -> ApiError {
    match error {
        ChatError::UnsupportedMessage { index, reason } => {
            field_error(&format!("messages[{index}]"), reason)
        }
        other => field_error("request", &other.to_string()),
    }
}

/// `prompt:<name>@<version>` or `span:<trace>:<span>`, and the prompt the run was made from.
fn source_of(source: Option<&PlaygroundSource>) -> (Option<String>, Option<(String, Option<i64>)>) {
    match source {
        None => (None, None),
        Some(PlaygroundSource::Prompt { name, version }) => (
            Some(format!("prompt:{name}@{version}")),
            Some((name.clone(), Some(*version))),
        ),
        Some(PlaygroundSource::Span { trace_id, span_id }) => (
            Some(format!("span:{}:{}", trace_id.to_hex(), span_id.to_hex())),
            None,
        ),
    }
}

/// A random trace id (D-V-37).
fn new_trace_id() -> Result<TraceId, ApiError> {
    let bytes = crate::auth::secret::random_bytes(16).map_err(internal)?;
    let bytes = <[u8; 16]>::try_from(bytes.as_slice()).map_err(internal)?;
    TraceId::from_bytes(bytes).map_err(internal)
}

fn internal(error: impl std::error::Error + Send + Sync + 'static) -> ApiError {
    ApiError::Internal(Box::new(error))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use axum::Json;
    use axum::http::{Method, StatusCode, header};
    use axum::routing::post;
    use cetra_capture::{Mock, Turn};
    use serde_json::{Value, json};

    use super::*;
    use crate::auth::roles::OrgRole;
    use crate::http::restore::tests::assert_refused;
    use crate::test_support::{Seeded, TestApp, TestResponse, TestStream, capture_logs};

    /// The request example (`fixtures/api/requests/playground-run.json`).
    const FIXTURE: &str = include_str!("../../../../fixtures/api/requests/playground-run.json");

    fn real_now() -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos(),
        )
        .expect("fits i64")
    }

    /// A seeded app whose clock is the real time, with private egress allowed for the mock.
    async fn new_app() -> (TestApp, Seeded) {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        app.state.clock.set(real_now());
        let seeded = app.seed().await;
        (app, seeded)
    }

    /// Serves `turns` on loopback as an OpenAI-style provider. Returns the mock and the base URL.
    async fn provider(turns: Vec<Turn>) -> (Arc<Mock>, String) {
        let mock = Mock::new(turns, Vec::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        let router = mock.router();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (mock, format!("http://{addr}/v1"))
    }

    fn chat_turn(text: &str, chunks: u32, input: u64, output: u64) -> Turn {
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"text": text},
            "finish": "stop",
            "usage": {"input": input, "output": output},
            "chunks": chunks,
            "ttft_ms": 0,
            "chunk_delay_ms": 0,
        }))
        .expect("a chat turn")
    }

    /// A turn that waits five seconds before it answers, so the run stays open.
    fn slow_turn() -> Turn {
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"text": "Late."},
            "finish": "stop",
            "usage": {"input": 1, "output": 1},
            "ttft_ms": 5000,
            "chunk_delay_ms": 0,
        }))
        .expect("a slow turn")
    }

    /// A turn whose assistant message holds `calls`.
    fn calls_turn(calls: &Value) -> Turn {
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"tool_calls": calls},
            "finish": "tool_calls",
            "usage": {"input": 5, "output": 2},
            "ttft_ms": 0,
            "chunk_delay_ms": 0,
        }))
        .expect("a tool turn")
    }

    /// Creates a connection to `base` in `org` (`llm-connections`, D-I-4) and returns its id.
    async fn create_connection(
        app: &TestApp,
        seeded: &Seeded,
        org: &str,
        base: &str,
        max_concurrency: u32,
        structured_output: &str,
    ) -> Uuid {
        let response = app
            .request(Method::POST, &format!("/api/v1/orgs/{org}/llm-connections"))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": format!("Mocked {}", Uuid::now_v7()),
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": base,
                "default_model": "gpt-test",
                "max_concurrency": max_concurrency,
                "requests_per_minute": 600,
                "structured_output": structured_output,
            }))
            .send()
            .await;
        assert_eq!(response.status, 201, "{}", text(&response));
        let value: Value = response.json();
        Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
    }

    fn run_path(seeded: &Seeded) -> String {
        format!("/api/v1/projects/{}/playground/runs", seeded.project.id)
    }

    /// The fixture's run, on `connection`, with the mock's model.
    fn run_body(connection: Uuid) -> Value {
        let mut body: Value = serde_json::from_str(FIXTURE).expect("fixture");
        body["connection_id"] = json!(connection);
        body["model"] = json!("gpt-test");
        body
    }

    /// Starts a run as the session user and returns the open stream.
    async fn stream_run(app: &TestApp, seeded: &Seeded, body: &Value) -> TestStream {
        app.request(Method::POST, &run_path(seeded))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(body)
            .send_stream()
            .await
    }

    /// Reads the stream to its end: the `(event, data)` pairs.
    async fn read_events(mut stream: TestStream) -> Vec<(String, Value)> {
        let mut events = Vec::new();
        while let Some(event) = stream.next_event(Duration::from_secs(10)).await {
            let data = serde_json::from_str(&event.data).unwrap_or(Value::Null);
            events.push((event.event, data));
        }
        events
    }

    fn names(events: &[(String, Value)]) -> Vec<&str> {
        events.iter().map(|event| event.0.as_str()).collect()
    }

    /// The `delta`s of the `text` events, joined.
    fn text_deltas(events: &[(String, Value)]) -> String {
        events
            .iter()
            .filter(|event| event.0 == "text")
            .map(|event| event.1["delta"].as_str().unwrap_or_default())
            .collect()
    }

    fn text(response: &TestResponse) -> String {
        String::from_utf8_lossy(&response.body).into_owned()
    }

    async fn list_traces(app: &TestApp, seeded: &Seeded, query: &str) -> Value {
        let path = format!("/api/v1/projects/{}/traces?{query}", seeded.project.id);
        let response = app
            .request(Method::GET, &path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        response.json()
    }

    /// The trace's detail, or `None` when it is not there.
    async fn trace_detail(app: &TestApp, seeded: &Seeded, trace_id: &str) -> Option<Value> {
        let path = format!("/api/v1/projects/{}/traces/{trace_id}", seeded.project.id);
        let response = app
            .request(Method::GET, &path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        (response.status == 200).then(|| response.json::<Value>())
    }

    #[tokio::test]
    async fn streams_a_run() {
        let (_mock, base) = provider(vec![chat_turn("Hello there, friend.", 3, 10, 3)]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let stream = stream_run(&app, &seeded, &run_body(connection)).await;
        assert_eq!(stream.status.as_u16(), 200);
        let events = read_events(stream).await;
        assert_eq!(names(&events), ["start", "text", "text", "text", "done"]);
        assert_eq!(text_deltas(&events), "Hello there, friend.");
        let done = &events[4].1;
        assert_eq!(done["usage"]["input"], 10);
        assert_eq!(
            done["output"]["parts"][0]["content"],
            "Hello there, friend."
        );

        // One trace in the reserved environment, with the rendered system message; the default
        // list leaves it out.
        let trace_id = events[0].1["trace_id"]
            .as_str()
            .expect("trace id")
            .to_owned();
        let detail = trace_detail(&app, &seeded, &trace_id)
            .await
            .expect("the playground trace");
        let span_id = detail["spans"][0]["span"]["span_id"]
            .as_str()
            .expect("span id");
        let span_path = format!(
            "/api/v1/projects/{}/traces/{trace_id}/spans/{span_id}",
            seeded.project.id
        );
        let span = app
            .request(Method::GET, &span_path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(span.status, 200, "{}", text(&span));
        assert!(text(&span).contains("for cetra."));
        let with_eval = list_traces(&app, &seeded, "include_eval=true").await;
        let playground = with_eval["data"]
            .as_array()
            .expect("rows")
            .iter()
            .filter(|row| row["environment"] == "cetra-playground")
            .count();
        assert_eq!(playground, 1);
        let default = list_traces(&app, &seeded, "").await;
        assert!(
            default["data"]
                .as_array()
                .expect("rows")
                .iter()
                .all(|row| row["environment"] != "cetra-playground")
        );
    }

    #[tokio::test]
    async fn missing_variable_is_400() {
        let (_mock, base) = provider(vec![]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let mut body = run_body(connection);
        body["variables"] = json!({});
        let response = app
            .request(Method::POST, &run_path(&seeded))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let problem: Value = response.json();
        assert_eq!(problem["errors"][0]["field"], "messages[0].parts[0]");
    }

    #[tokio::test]
    async fn render_variables_on_renders_and_refuses_a_missing_variable() {
        let (_mock, base) = provider(vec![]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let mut body = run_body(connection);
        // The default is true: the same as sending `render_variables: true`.
        body["render_variables"] = json!(true);
        body["variables"] = json!({});
        body["messages"] = json!([
            {"role": "user", "parts": [{"type": "text", "content": "Hi {{absent}}"}]}
        ]);
        let response = app
            .request(Method::POST, &run_path(&seeded))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let problem: Value = response.json();
        assert_eq!(problem["errors"][0]["field"], "messages[0].parts[0]");
    }

    #[tokio::test]
    async fn render_variables_off_sends_the_text_as_written() {
        let (_mock, base) = provider(vec![chat_turn("Fine.", 1, 4, 1)]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let mut body = run_body(connection);
        body["render_variables"] = json!(false);
        body["variables"] = json!({});
        body["messages"] = json!([
            {"role": "user", "parts": [{"type": "text", "content": "Hi {{absent}}"}]}
        ]);
        let events = read_events(stream_run(&app, &seeded, &body).await).await;
        assert_eq!(names(&events).last().copied(), Some("done"), "{events:?}");
        let trace_id = events[0].1["trace_id"].as_str().expect("trace id");
        let detail = trace_detail(&app, &seeded, trace_id)
            .await
            .expect("the playground trace");
        let span_id = detail["spans"][0]["span"]["span_id"]
            .as_str()
            .expect("span id");
        let span_path = format!(
            "/api/v1/projects/{}/traces/{trace_id}/spans/{span_id}",
            seeded.project.id
        );
        let span = app
            .request(Method::GET, &span_path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(span.status, 200, "{}", text(&span));
        assert!(text(&span).contains("Hi {{absent}}"), "{}", text(&span));
    }

    #[tokio::test]
    async fn connection_of_another_org_is_400() {
        let (_mock, base) = provider(vec![]).await;
        let (app, seeded) = new_app().await;
        let created = app
            .request(Method::POST, "/api/v1/orgs")
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({"name": "Other", "slug": "other"}))
            .send()
            .await;
        assert_eq!(created.status, 201, "{}", text(&created));
        let foreign = create_connection(&app, &seeded, "other", &base, 2, "native").await;
        let response = app
            .request(Method::POST, &run_path(&seeded))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&run_body(foreign))
            .send()
            .await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let problem: Value = response.json();
        assert_eq!(problem["errors"][0]["field"], "connection_id");
    }

    #[tokio::test]
    async fn viewer_and_keys_are_refused() {
        let (_mock, base) = provider(vec![chat_turn("Yes.", 1, 1, 1)]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let body = run_body(connection);
        let path = run_path(&seeded);

        let viewer = app
            .add_member(&seeded, "viewer@example.com", OrgRole::Viewer)
            .await;
        let refused = app
            .request(Method::POST, &path)
            .header(header::COOKIE.as_str(), &viewer.cookie)
            .json(&body)
            .send()
            .await;
        assert_eq!(refused.status, 403, "{}", text(&refused));

        let key = app
            .request(Method::POST, &path)
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {}", seeded.read_key),
            )
            .json(&body)
            .send()
            .await;
        assert_eq!(key.status, 403, "{}", text(&key));

        let read_only = app.personal_token(seeded.user_id, false).await;
        let refused = app
            .request(Method::POST, &path)
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {read_only}"),
            )
            .json(&body)
            .send()
            .await;
        assert_eq!(refused.status, 403, "{}", text(&refused));

        let read_write = app.personal_token(seeded.user_id, true).await;
        let stream = app
            .request(Method::POST, &path)
            .header(
                header::AUTHORIZATION.as_str(),
                &format!("Bearer {read_write}"),
            )
            .json(&body)
            .send_stream()
            .await;
        assert_eq!(stream.status.as_u16(), 200);
        let events = read_events(stream).await;
        assert_eq!(names(&events).last().copied(), Some("done"));
    }

    #[tokio::test]
    async fn a_busy_connection_answers_429_after_the_wait() {
        let (_mock, base) = provider(vec![]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 1, "native").await;
        let row = connections::get_connection(app.state.meta.reader(), seeded.org_id, connection)
            .await
            .expect("query")
            .expect("the connection");
        // Holds the only slot; the run waits for it, then answers 429.
        let held = interactive::acquire(&app.state, &row, Duration::from_secs(1))
            .await
            .expect("the slot");
        let started = std::time::Instant::now();
        let refused = app
            .request(Method::POST, &run_path(&seeded))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&run_body(connection))
            .send()
            .await;
        assert_eq!(refused.status, 429, "{}", text(&refused));
        assert!(started.elapsed() >= Duration::from_secs(9));
        drop(held);
    }

    #[tokio::test]
    async fn the_fifth_run_of_a_user_answers_429() {
        let (_mock, base) = provider((0..4).map(|_| slow_turn()).collect()).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 8, "native").await;
        let body = run_body(connection);
        let mut held = Vec::new();
        for _ in 0..4 {
            let stream = stream_run(&app, &seeded, &body).await;
            assert_eq!(stream.status.as_u16(), 200);
            held.push(stream);
        }
        let fifth = stream_run(&app, &seeded, &body).await;
        assert_eq!(fifth.status.as_u16(), 429);
        drop(held);
    }

    #[tokio::test]
    async fn cancel_drops_the_provider_call_and_records_the_run() {
        let (_mock, base) = provider(vec![slow_turn()]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let mut stream = stream_run(&app, &seeded, &run_body(connection)).await;
        let start = stream
            .next_event(Duration::from_secs(5))
            .await
            .expect("the start event");
        assert_eq!(start.event, "start");
        let started: Value = serde_json::from_str(&start.data).expect("json");
        let trace_id = started["trace_id"].as_str().expect("trace id").to_owned();
        // The client leaves while the provider is still waiting.
        drop(stream);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(detail) = trace_detail(&app, &seeded, &trace_id).await
                && detail["trace"]["status"] == "error"
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the cancelled run was not traced with status error"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn tool_calls_and_response_format() {
        let (_mock, base) = provider(vec![
            calls_turn(&json!([{"id": "call_1", "name": "lookup", "arguments": {"q": "refund"}}])),
            calls_turn(
                &json!([{"id": "call_2", "name": "submit_answer", "arguments": {"answer": "yes"}}]),
            ),
        ])
        .await;
        let (app, seeded) = new_app().await;
        let native = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let tool_mode = create_connection(&app, &seeded, "acme", &base, 2, "tool").await;

        let mut body = run_body(native);
        body["tools"] = json!([{"name": "lookup", "description": "Find it.", "parameters": {"type": "object"}}]);
        body["tool_choice"] = json!("auto");
        let events = read_events(stream_run(&app, &seeded, &body).await).await;
        let call = events
            .iter()
            .find(|event| event.0 == "tool_call")
            .expect("a tool_call event");
        assert_eq!(call.1["name"], "lookup");

        let mut body = run_body(tool_mode);
        body["response_format"] =
            json!({"name": "answer", "schema": {"type": "object"}, "strict": true});
        let events = read_events(stream_run(&app, &seeded, &body).await).await;
        assert_eq!(text_deltas(&events), r#"{"answer":"yes"}"#);
        assert!(events.iter().all(|event| event.0 != "tool_call"));
        // The provider said `tool_calls` for the answer tool, which was streamed as text: the run
        // stopped normally (D-V-25).
        let done = events.last().expect("events");
        assert_eq!(done.0, "done");
        assert_eq!(done.1["finish_reason"], "stop");
        assert_eq!(done.1["output"]["finish_reason"], "stop");
    }

    #[tokio::test]
    async fn provider_error_is_an_error_event() {
        let stub = axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": {
                        "message": "Invalid API key.",
                        "type": "invalid_request_error",
                        "code": "invalid_api_key"
                    }})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, stub).await;
        });
        let (app, seeded) = new_app().await;
        let connection = create_connection(
            &app,
            &seeded,
            "acme",
            &format!("http://{addr}/v1"),
            2,
            "native",
        )
        .await;
        let events = read_events(stream_run(&app, &seeded, &run_body(connection)).await).await;
        let last = events.last().expect("events");
        assert_eq!(last.0, "error");
        assert_eq!(last.1["class"], "pause");
        // D-V-36: the run does not mark the connection failing.
        let response = app
            .request(
                Method::GET,
                &format!("/api/v1/orgs/acme/llm-connections/{connection}"),
            )
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(response.status, 200, "{}", text(&response));
        let value: Value = response.json();
        assert_eq!(value["status"], "ok");
    }

    #[tokio::test]
    async fn restore_mode_refuses() {
        let app = TestApp::with_config(|config| config.restore_mode = true).await;
        let seeded = app.seed().await;
        let response = app
            .request(Method::POST, &run_path(&seeded))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&run_body(Uuid::nil()))
            .send()
            .await;
        assert_refused(&response, "playground run");
    }

    #[tokio::test]
    async fn audit_log_has_no_payload() {
        let (_mock, base) = provider(vec![chat_turn("Ok.", 1, 1, 1)]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let mut body = run_body(connection);
        body["messages"][1]["parts"][0]["content"] = json!("canary-text");
        let (events, logs) =
            capture_logs(async { read_events(stream_run(&app, &seeded, &body).await).await }).await;
        assert_eq!(names(&events).last().copied(), Some("done"));
        assert!(logs.contains("playground run"), "{logs}");
        assert!(!logs.contains("canary-text"), "{logs}");
    }

    /// Serves `router` on loopback and returns the provider's base URL.
    async fn serve(router: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn placeholders_expand_into_the_trace() {
        let (_mock, base) = provider(vec![chat_turn("Noted.", 1, 4, 1)]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let mut body = run_body(connection);
        body["messages"] = json!([
            {"type": "placeholder", "name": "history"},
            {"role": "user", "parts": [{"type": "text", "content": "Next: {{product}}"}]}
        ]);
        body["placeholders"] = json!({"history": [
            {"role": "user", "parts": [{"type": "text", "content": "earlier question"}]}
        ]});
        let events = read_events(stream_run(&app, &seeded, &body).await).await;
        assert_eq!(names(&events).last().copied(), Some("done"));
        let trace_id = events[0].1["trace_id"].as_str().expect("trace id");
        let detail = trace_detail(&app, &seeded, trace_id)
            .await
            .expect("the playground trace");
        let span_id = detail["spans"][0]["span"]["span_id"]
            .as_str()
            .expect("span id");
        let span_path = format!(
            "/api/v1/projects/{}/traces/{trace_id}/spans/{span_id}",
            seeded.project.id
        );
        let span = app
            .request(Method::GET, &span_path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(span.status, 200, "{}", text(&span));
        let input = text(&span);
        assert!(input.contains("earlier question"), "{input}");
        assert!(input.contains("Next: cetra"), "{input}");

        // A placeholder with no entry is a 400 before the stream starts.
        body["placeholders"] = json!({});
        let response = app
            .request(Method::POST, &run_path(&seeded))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let problem: Value = response.json();
        assert_eq!(problem["errors"][0]["field"], "placeholders");
    }

    #[tokio::test]
    async fn rendered_text_over_4_mib_is_400() {
        let (_mock, base) = provider(vec![]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let mut body = run_body(connection);
        body["variables"] = json!({"big": "x".repeat(1 << 20)});
        body["messages"] = json!([
            {"role": "user", "parts": [{"type": "text", "content": "{{big}}{{big}}{{big}}{{big}}{{big}}"}]}
        ]);
        let response = app
            .request(Method::POST, &run_path(&seeded))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&body)
            .send()
            .await;
        assert_eq!(response.status, 400, "{}", text(&response));
        let problem: Value = response.json();
        assert_eq!(problem["errors"][0]["field"], "messages");
        assert_eq!(
            problem["errors"][0]["message"],
            "the rendered messages are larger than 4 MiB"
        );
    }

    #[tokio::test]
    async fn source_names_are_bounded() {
        let (_mock, base) = provider(vec![]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let cases = [
            (json!({"prompt": {"name": "", "version": 1}}), "source.name"),
            (
                json!({"prompt": {"name": "a b", "version": 1}}),
                "source.name",
            ),
            (
                json!({"prompt": {"name": "a".repeat(201), "version": 1}}),
                "source.name",
            ),
            (
                json!({"prompt": {"name": "support-reply", "version": 0}}),
                "source.version",
            ),
        ];
        for (source, field) in cases {
            let mut body = run_body(connection);
            body["source"] = source;
            let response = app
                .request(Method::POST, &run_path(&seeded))
                .header(header::COOKIE.as_str(), &seeded.cookie)
                .json(&body)
                .send()
                .await;
            assert_eq!(response.status, 400, "{}", text(&response));
            let problem: Value = response.json();
            assert_eq!(problem["errors"][0]["field"], field);
        }
    }

    #[tokio::test]
    async fn the_org_answers_429_past_sixteen_runs() {
        let (app, seeded) = new_app().await;
        // The connection is never called: admission refuses the run first.
        let connection =
            create_connection(&app, &seeded, "acme", "http://127.0.0.1:9/v1", 16, "native").await;
        let now = app.state.clock.now_ns();
        // Sixteen runs of other users fill the org's slots.
        let mut held = Vec::new();
        for _ in 0..16 {
            let guard = app
                .state
                .limits
                .playground
                .admit(Uuid::now_v7(), seeded.org_id, now)
                .unwrap_or_else(|_| panic!("an org slot"));
            held.push(guard);
        }
        let refused = app
            .request(Method::POST, &run_path(&seeded))
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&run_body(connection))
            .send()
            .await;
        assert_eq!(refused.status, 429, "{}", text(&refused));
        drop(held);
    }

    #[tokio::test]
    async fn a_client_leaving_before_the_head_cancels_the_run() {
        // The provider sleeps before it answers, so the run has no response head yet.
        let base = serve(axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Json(json!({}))
            }),
        ))
        .await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let mut stream = stream_run(&app, &seeded, &run_body(connection)).await;
        let start = stream
            .next_event(Duration::from_secs(5))
            .await
            .expect("the start event");
        let started: Value = serde_json::from_str(&start.data).expect("json");
        let trace_id = started["trace_id"].as_str().expect("trace id").to_owned();
        drop(stream);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(detail) = trace_detail(&app, &seeded, &trace_id).await
                && detail["trace"]["status"] == "error"
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the run waiting for the head was not traced as cancelled"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn a_silent_provider_ends_the_run_by_the_idle_timeout() {
        // One chunk, then the provider stays silent: the run ends after RUN_IDLE of silence.
        // Pings do not restart the provider's idle timeout (see `read_stream`).
        let chunk = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"gpt-test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n";
        let base = serve(axum::Router::new().route(
            "/v1/chat/completions",
            post(move || async move {
                let first = futures::stream::iter(vec![Ok::<Vec<u8>, std::io::Error>(
                    chunk.as_bytes().to_vec(),
                )]);
                let body = futures::StreamExt::chain(first, futures::stream::pending());
                Response::builder()
                    .header(header::CONTENT_TYPE.as_str(), "text/event-stream")
                    .body(axum::body::Body::from_stream(body))
                    .expect("a response")
            }),
        ))
        .await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let started = std::time::Instant::now();
        let events = read_events(stream_run(&app, &seeded, &run_body(connection)).await).await;
        assert_eq!(names(&events).last().copied(), Some("error"), "{events:?}");
        assert!(
            started.elapsed() >= RUN_IDLE,
            "the run ended before the idle timeout"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_waits_for_a_running_runs_trace() {
        let (_mock, base) = provider(vec![slow_turn()]).await;
        let (app, seeded) = new_app().await;
        let connection = create_connection(&app, &seeded, "acme", &base, 2, "native").await;
        let mut stream = stream_run(&app, &seeded, &run_body(connection)).await;
        let start = stream
            .next_event(Duration::from_secs(5))
            .await
            .expect("the start event");
        let started: Value = serde_json::from_str(&start.data).expect("json");
        let trace_id = started["trace_id"].as_str().expect("trace id").to_owned();
        // The shutdown signal ends the run while the provider is still waiting. The run task
        // holds the commits guard, so the wait returns only after its trace is committed.
        app.state.live.close();
        tokio::time::timeout(Duration::from_secs(10), app.state.ingest.wait_for_commits())
            .await
            .expect("the run's commit finishes");
        let detail = trace_detail(&app, &seeded, &trace_id)
            .await
            .expect("the run's trace is committed before shutdown returns");
        assert_eq!(detail["trace"]["status"], "error");
        drop(stream);
    }
}
