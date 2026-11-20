//! The assistant's turn engine (track AS M3, D-AS-8, D-AS-9, D-AS-15, D-AS-21 to D-AS-23): the loop
//! that calls the model through the org's connection, runs the M2 tools, stores every message and
//! traces the turn. Events go to a channel; the route turns them into SSE (AS3.6).
//!
//! Logs, errors, `Debug` output and trace attributes never hold message text, tool arguments or
//! results, drafts, page context or model answers (AGENTS.md § Privacy).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use cetra_core::enums::SpanKind;
use cetra_core::ids::TraceId;
use cetra_eval::assistant_trace::{
    AssistantTurnTrace, ModelCallRecord, ToolCallRecord, build_assistant_trace,
};
use cetra_eval::{
    CHAT_IDLE_TIMEOUT, CHAT_TIMEOUT_MAX, ChatDone, ChatError, ChatEvent, ChatParams, ChatRequest,
    EgressError, ErrorClass, JudgeConnection, ToolCall, ToolChoice, ToolDefinition, stream_chat,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use uuid::Uuid;

use super::history;
use super::tools::{self, ToolResult, ToolScope, ToolSpec, UnknownTool, specs};
use super::{PROMPT_VERSION, STUB_AFTER_NS, SYSTEM_PROMPT};
use crate::api::assistant::{AssistantDraftKind, AssistantPart};
use crate::auth::limits::AssistantGuard;
use crate::auth::principal::AuthUser;
use crate::eval::execute::commit_judge_trace;
use crate::eval::interactive::{self, Busy};
use crate::http::AppState;
use crate::live::shut;
use crate::meta::MetaError;
use crate::meta::assistant::{
    MessageRole, MessageRow, MessageStatus, NewMessage, TOO_MANY_MESSAGES, append_message,
    get_conversation_with_messages,
};
use crate::meta::connections::ConnectionRow;
use crate::meta::projects::ProjectRow;

/// Per turn (D-AS-15).
pub(crate) const MAX_MODEL_CALLS: usize = 16;
pub(crate) const MAX_TOOL_CALLS: usize = 32;
pub(crate) const TURN_DEADLINE: Duration = Duration::from_secs(600);
pub(crate) const MAX_TOKENS: u32 = 4_096;
pub(crate) const PERMIT_WAIT: Duration = Duration::from_secs(10);
/// The most messages one turn can append (D-AS-25): the user message, up to 16 assistant messages
/// and 16 tool messages, and the closing limit message. A turn starts only when the conversation
/// has room for this many.
pub(crate) const TURN_MESSAGES_MAX: i64 = 34;

/// The shortest whole-call timeout a model call gets, when the turn is nearly over.
const MIN_CALL_TIMEOUT: Duration = Duration::from_secs(1);

/// Error classes, as the SSE `error` event names them (`ErrorClass`).
const RETRYABLE: &str = "retryable";
const PERMANENT: &str = "permanent";
/// Trace and audit text of a cancelled turn or call.
const CANCELLED: &str = "cancelled";
const BUSY: &str = "the LLM connection is busy";
const CONVERSATION_NOT_FOUND: &str = "conversation not found";
const INTERNAL: &str = "internal error";

/// The limit texts the server appends (D-AS-23).
const LIMIT_CALLS: &str =
    "I stopped here: this turn reached its limit of 16 model calls. Ask me to continue.";
const LIMIT_TIME: &str =
    "I stopped here: this turn reached its limit of 10 minutes. Ask me to continue.";
const LIMIT_TOOLS: &str =
    "I stopped here: this turn reached its limit of 32 tool calls. Ask me to continue.";
/// The result of a tool call the limit kept from running (D-AS-23).
const NOT_RUN_LIMIT: &str = "not run: the turn's limit of 32 tool calls was reached";

/// What the route hands the task.
pub(crate) struct TurnSetup {
    pub(crate) project: ProjectRow,
    /// From `meta::orgs::org_by_id(state.meta.reader(), project.org_id)` in the route.
    pub(crate) org_slug: String,
    pub(crate) user: AuthUser,
    pub(crate) conversation: Uuid,
    /// The sequence number of the stored user message.
    pub(crate) user_seq: i64,
    pub(crate) context: Option<Value>,
    pub(crate) connection_row: ConnectionRow,
    pub(crate) connection: Arc<JudgeConnection>,
    pub(crate) model: String,
    pub(crate) trace_id: TraceId,
    /// How long a model call waits for the connection (the route sets [`PERMIT_WAIT`]; tests shorten it).
    pub(crate) permit_wait: Duration,
    /// How long the turn may run before its time limit (the route sets [`TURN_DEADLINE`]; tests shorten it).
    pub(crate) turn_deadline: Duration,
}

/// What the turn emits; the route turns each into an SSE event named `name()` whose data is
/// `data()` (AS3.6). Not `Serialize` itself; `Debug` is hand-written: the variant name and sizes only.
#[derive(Clone, PartialEq)]
pub(crate) enum TurnEvent {
    Start {
        turn_id: i64,
        trace_id: String,
    },
    Text {
        delta: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        id: String,
        ok: bool,
        summary: ToolSummary,
    },
    Draft {
        id: String,
        kind: AssistantDraftKind,
        draft: Value,
    },
    Done {
        finish_reason: String,
        usage: TurnUsage,
        cost_usd: Option<f64>,
        steps: usize,
    },
    Error {
        class: String,
        message: String,
    },
}

/// What the route sends about one tool result. Sizes and codes only.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct ToolSummary {
    pub(crate) size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) rows: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error_code: Option<String>,
}

/// The tokens of a whole turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub(crate) struct TurnUsage {
    pub(crate) input: u64,
    pub(crate) output: u64,
}

impl TurnEvent {
    /// `start`, `text`, `tool_call`, `tool_result`, `draft`, `done` or `error`.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::Text { .. } => "text",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Draft { .. } => "draft",
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
        }
    }

    /// The variant's fields as a JSON object (field names as written above; `trace_id` as hex).
    pub(crate) fn data(&self) -> Value {
        match self {
            Self::Start { turn_id, trace_id } => {
                json!({"turn_id": turn_id, "trace_id": trace_id})
            }
            Self::Text { delta } => json!({"delta": delta}),
            Self::ToolCall {
                id,
                name,
                arguments,
            } => json!({"id": id, "name": name, "arguments": arguments}),
            Self::ToolResult { id, ok, summary } => {
                json!({"id": id, "ok": ok, "summary": summary})
            }
            Self::Draft { id, kind, draft } => json!({"id": id, "kind": kind, "draft": draft}),
            Self::Done {
                finish_reason,
                usage,
                cost_usd,
                steps,
            } => json!({
                "finish_reason": finish_reason,
                "usage": usage,
                "cost_usd": cost_usd,
                "steps": steps,
            }),
            Self::Error { class, message } => json!({"class": class, "message": message}),
        }
    }
}

/// Prints the event's name, and for `Text` the length of the delta, never its text.
impl fmt::Debug for TurnEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { delta } => formatter
                .debug_struct("Text")
                .field("bytes", &delta.len())
                .finish(),
            other => formatter.write_str(other.name()),
        }
    }
}

/// Runs one turn to its end. `sink` receives the events; when its receiver is dropped the turn is
/// cancelled. Never returns an error: failures end the turn with an `Error` event.
pub(crate) async fn run_turn(
    state: AppState,
    setup: TurnSetup,
    sink: mpsc::Sender<TurnEvent>,
    _guard: AssistantGuard,
) {
    let started_ns = state.clock.now_ns();
    let mut turn = Turn {
        state: &state,
        setup: &setup,
        sink: &sink,
        closed: state.live.closed(),
        trace_hex: setup.trace_id.to_hex(),
        started_ns,
        deadline: Instant::now() + setup.turn_deadline,
        system: String::new(),
        rows: Vec::new(),
        model_calls: Vec::new(),
        tool_calls: Vec::new(),
        usage: TurnUsage::default(),
        tool_count: 0,
    };
    let outcome = turn.run().await;
    turn.finish(outcome).await;
}

/// How the turn ended. Carries payload-free text only.
enum Outcome {
    /// The model's last answer, with its finish reason.
    Done(String),
    /// A limit ended the turn (D-AS-23).
    Limit,
    /// The turn failed: a class (`retryable`, `pause` or `permanent`) and a message.
    Error {
        class: &'static str,
        message: String,
    },
    Cancelled,
}

impl Outcome {
    fn error(class: &'static str, message: impl Into<String>) -> Self {
        Self::Error {
            class,
            message: message.into(),
        }
    }
}

/// What one model call left for the loop.
struct Step {
    calls: Vec<ToolCall>,
    done: Option<ChatDone>,
    cancelled: bool,
}

/// How the tool calls of one step ended.
enum Tools {
    Continue,
    /// The 32-call limit was reached: the turn ends after the limit's text.
    Limit,
    Cancelled,
}

/// A message to store. `stubs` gives a tool message its stub time (D-AS-12).
struct Stored {
    role: MessageRole,
    parts: Vec<AssistantPart>,
    status: MessageStatus,
    stubs: bool,
}

/// The state of one running turn.
struct Turn<'a> {
    state: &'a AppState,
    setup: &'a TurnSetup,
    sink: &'a mpsc::Sender<TurnEvent>,
    /// Closes when the server shuts down.
    closed: watch::Receiver<bool>,
    trace_hex: String,
    started_ns: i64,
    deadline: Instant,
    system: String,
    /// The conversation's messages as stored, including those of this turn.
    rows: Vec<MessageRow>,
    model_calls: Vec<ModelCallRecord>,
    tool_calls: Vec<ToolCallRecord>,
    usage: TurnUsage,
    tool_count: usize,
}

impl Turn<'_> {
    /// Sends one event. `false` when the receiver is gone, which is a cancel.
    async fn emit(&self, event: TurnEvent) -> bool {
        self.sink.send(event).await.is_ok()
    }

    /// The events and messages of the turn, up to its outcome.
    async fn run(&mut self) -> Outcome {
        let start = TurnEvent::Start {
            turn_id: self.setup.user_seq,
            trace_id: self.trace_hex.clone(),
        };
        if !self.emit(start).await {
            return Outcome::Cancelled;
        }
        let loaded = get_conversation_with_messages(
            self.state.meta.reader(),
            self.setup.project.id,
            self.setup.user.id,
            self.setup.conversation,
        )
        .await;
        self.rows = match loaded {
            Ok(Some((_, rows))) => rows,
            Ok(None) => return Outcome::error(PERMANENT, CONVERSATION_NOT_FOUND),
            Err(error) => {
                tracing::error!(
                    conversation_id = %self.setup.conversation,
                    trace_id = %self.trace_hex,
                    error_class = meta_error_class(&error),
                    "assistant turn could not load its conversation"
                );
                return internal();
            }
        };
        self.system = system_message(self.setup, self.state.clock.now_ns());
        loop {
            if self.model_calls.len() >= MAX_MODEL_CALLS {
                return self.limit(LIMIT_CALLS).await;
            }
            if Instant::now() >= self.deadline {
                return self.limit(LIMIT_TIME).await;
            }
            let request = match self.request() {
                Ok(request) => request,
                Err(outcome) => return outcome,
            };
            let step = match self.model_call(request).await {
                Ok(step) => step,
                // The call ran past the turn's deadline: the same limit as the top of the loop (D-AS-23).
                Err(Outcome::Limit) => return self.limit(LIMIT_TIME).await,
                Err(outcome) => return outcome,
            };
            if step.cancelled {
                return Outcome::Cancelled;
            }
            if step.calls.is_empty() {
                let finish_reason = step
                    .done
                    .and_then(|done| done.finish_reason)
                    .map_or_else(|| "stop".to_owned(), |reason| reason.as_str().to_owned());
                return Outcome::Done(finish_reason);
            }
            match self.run_calls(&step.calls).await {
                Ok(Tools::Continue) => {}
                Ok(Tools::Limit) => return self.limit(LIMIT_TOOLS).await,
                Ok(Tools::Cancelled) => return Outcome::Cancelled,
                Err(outcome) => return outcome,
            }
        }
    }

    /// The provider messages and the call's settings (D-AS-15, D-AS-22).
    fn request(&self) -> Result<ChatRequest, Outcome> {
        let messages = history::provider_messages(&self.system, &self.rows)
            .map_err(|error| Outcome::error(PERMANENT, error.to_string()))?;
        let timeout = self
            .deadline
            .saturating_duration_since(Instant::now())
            .clamp(MIN_CALL_TIMEOUT, CHAT_TIMEOUT_MAX);
        Ok(ChatRequest {
            model: self.setup.model.clone(),
            messages,
            tools: definitions(specs()),
            tool_choice: Some(ToolChoice::Auto),
            response_format: None,
            params: ChatParams::default(),
            max_tokens: MAX_TOKENS,
            timeout,
            idle_timeout: CHAT_IDLE_TIMEOUT,
        })
    }

    /// One model call: waits for the connection's permit, streams the answer, then records and
    /// stores the assistant message. A failed call stores its partial message and returns its
    /// outcome; a cancelled call returns a cancelled step.
    async fn model_call(&mut self, request: ChatRequest) -> Result<Step, Outcome> {
        let started_ns = self.state.clock.now_ns();
        let admitted = tokio::select! {
            biased;
            () = shut(&mut self.closed) => None,
            () = self.sink.closed() => None,
            result = interactive::acquire(self.state, &self.setup.connection_row, self.setup.permit_wait) => Some(result),
        };
        let permit = match admitted {
            None => {
                let record = bare_record(started_ns, self.state.clock.now_ns(), CANCELLED);
                self.settle(record, "", &[], MessageStatus::Cancelled)
                    .await?;
                return Ok(Step {
                    calls: Vec::new(),
                    done: None,
                    cancelled: true,
                });
            }
            Some(Err(Busy)) => {
                let record = bare_record(started_ns, self.state.clock.now_ns(), BUSY);
                self.settle(record, "", &[], MessageStatus::Failed).await?;
                return Err(Outcome::error(RETRYABLE, BUSY));
            }
            Some(Ok(permit)) => permit,
        };

        let opened = tokio::select! {
            biased;
            () = shut(&mut self.closed) => None,
            () = self.sink.closed() => None,
            result = stream_chat(self.state.eval_runner.egress(), self.setup.connection.as_ref(), &request) => Some(result),
        };
        let mut stream = match opened {
            None => {
                drop(permit);
                let record = bare_record(started_ns, self.state.clock.now_ns(), CANCELLED);
                self.settle(record, "", &[], MessageStatus::Cancelled)
                    .await?;
                return Ok(Step {
                    calls: Vec::new(),
                    done: None,
                    cancelled: true,
                });
            }
            Some(Err(error)) => {
                drop(permit);
                let record = bare_record(started_ns, self.state.clock.now_ns(), error.to_string());
                let status = if self.past_deadline(&error) {
                    MessageStatus::Cancelled
                } else {
                    MessageStatus::Failed
                };
                self.settle(record, "", &[], status).await?;
                return Err(self.call_outcome(&error));
            }
            Some(Ok(stream)) => stream,
        };

        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut done: Option<ChatDone> = None;
        let mut ttft_ns: Option<i64> = None;
        let mut failure: Option<ChatError> = None;
        let mut cancelled = false;
        loop {
            tokio::select! {
                biased;
                () = shut(&mut self.closed) => {
                    cancelled = true;
                    break;
                }
                () = self.sink.closed() => {
                    cancelled = true;
                    break;
                }
                next = stream.next() => match next {
                    Ok(Some(ChatEvent::Text(delta))) => {
                        if ttft_ns.is_none() {
                            ttft_ns = Some(self.state.clock.now_ns().saturating_sub(started_ns));
                        }
                        text.push_str(&delta);
                        if !self.emit(TurnEvent::Text { delta }).await {
                            cancelled = true;
                            break;
                        }
                    }
                    Ok(Some(ChatEvent::ToolCall(call))) => {
                        if ttft_ns.is_none() {
                            ttft_ns = Some(self.state.clock.now_ns().saturating_sub(started_ns));
                        }
                        let event = TurnEvent::ToolCall {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        };
                        calls.push(call);
                        if !self.emit(event).await {
                            cancelled = true;
                            break;
                        }
                    }
                    Ok(Some(ChatEvent::Done(finished))) => {
                        done = Some(finished);
                        break;
                    }
                    Ok(None) => break,
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
        }
        drop(permit);
        drop(stream);

        let record = ModelCallRecord {
            started_ns,
            ended_ns: self.state.clock.now_ns(),
            response_model: done
                .as_ref()
                .and_then(|finished| finished.response_model.clone()),
            usage: done.as_ref().and_then(|finished| finished.usage),
            finish_reason: done
                .as_ref()
                .and_then(|finished| finished.finish_reason.as_ref())
                .map(|reason| reason.as_str().to_owned()),
            ttft_ns,
            error: if cancelled {
                Some(CANCELLED.to_owned())
            } else {
                failure.as_ref().map(|error| error.to_string())
            },
        };
        // A call that ran into the turn's deadline keeps its partial answer as cancelled, then the
        // turn stores the limit message (D-AS-23).
        let status = if cancelled
            || failure
                .as_ref()
                .is_some_and(|error| self.past_deadline(error))
        {
            MessageStatus::Cancelled
        } else if failure.is_some() {
            MessageStatus::Failed
        } else {
            MessageStatus::Complete
        };
        self.settle(record, &text, &calls, status).await?;
        if cancelled {
            return Ok(Step {
                calls,
                done,
                cancelled: true,
            });
        }
        if let Some(error) = failure {
            return Err(self.call_outcome(&error));
        }
        Ok(Step {
            calls,
            done,
            cancelled: false,
        })
    }

    /// Whether a call failed on its whole-call timeout, which the clamp to the turn's time left
    /// makes the deadline, and the deadline has passed (D-AS-23).
    fn past_deadline(&self, error: &ChatError) -> bool {
        matches!(error, ChatError::Transport(EgressError::Timeout))
            && Instant::now() >= self.deadline
    }

    /// The outcome of a failed call. A timeout past the turn's deadline is the time limit
    /// (D-AS-23); any other failure is its class and its payload-free text.
    fn call_outcome(&self, error: &ChatError) -> Outcome {
        if self.past_deadline(error) {
            return Outcome::Limit;
        }
        Outcome::error(class_name(error.class()), error.to_string())
    }

    /// Records one model call and stores its assistant message.
    async fn settle(
        &mut self,
        record: ModelCallRecord,
        text: &str,
        calls: &[ToolCall],
        status: MessageStatus,
    ) -> Result<(), Outcome> {
        if let Some(usage) = record.usage {
            self.usage.input = self.usage.input.saturating_add(usage.input);
            self.usage.output = self.usage.output.saturating_add(usage.output);
        }
        self.model_calls.push(record);
        self.store(Stored {
            role: MessageRole::Assistant,
            parts: assistant_parts(text, calls),
            status,
            stubs: false,
        })
        .await
    }

    /// Runs the step's tool calls in order and stores their tool message (D-AS-21).
    async fn run_calls(&mut self, calls: &[ToolCall]) -> Result<Tools, Outcome> {
        let model_index = self.model_calls.len().saturating_sub(1);
        let scope = ToolScope {
            user: self.setup.user.clone(),
            project: self.setup.project.id,
        };
        let mut parts: Vec<AssistantPart> = Vec::new();
        let mut limited = false;
        let mut cancelled = false;
        for call in calls {
            let started_ns = self.state.clock.now_ns();
            let result = if self.tool_count >= MAX_TOOL_CALLS {
                limited = true;
                Some(not_run(NOT_RUN_LIMIT))
            } else {
                self.tool_count += 1;
                tokio::select! {
                    biased;
                    () = shut(&mut self.closed) => None,
                    () = self.sink.closed() => None,
                    result = run_tool_call(self.state, &scope, call) => Some(result),
                }
            };
            let Some(result) = result else {
                cancelled = true;
                break;
            };
            let ended_ns = self.state.clock.now_ns();
            let size_bytes = serde_json::to_string(&result.value)
                .map_or(0, |text| u64::try_from(text.len()).unwrap_or(u64::MAX));
            let error_code = if result.ok {
                None
            } else {
                result.value["error"]["code"].as_str().map(str::to_owned)
            };
            let ToolResult {
                ok,
                value,
                rows,
                draft,
            } = result;
            self.tool_calls.push(ToolCallRecord {
                name: trace_tool_name(&call.name).to_owned(),
                started_ns,
                ended_ns,
                size_bytes,
                rows,
                error_code: error_code.clone(),
                call: model_index,
            });
            parts.push(AssistantPart::ToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                ok,
                result: Some(value),
                size_bytes,
                rows,
                stubbed: false,
            });
            let summary = ToolSummary {
                size_bytes,
                rows,
                error_code,
            };
            if !self
                .emit(TurnEvent::ToolResult {
                    id: call.id.clone(),
                    ok,
                    summary,
                })
                .await
            {
                cancelled = true;
                break;
            }
            if let Some((kind, draft)) = draft {
                let id = format!("draft_{}", call.id);
                parts.push(AssistantPart::Draft {
                    id: id.clone(),
                    kind,
                    draft: draft.clone(),
                });
                if !self.emit(TurnEvent::Draft { id, kind, draft }).await {
                    cancelled = true;
                    break;
                }
            }
        }
        if !parts.is_empty() {
            self.store(Stored {
                role: MessageRole::Tool,
                parts,
                status: MessageStatus::Complete,
                stubs: true,
            })
            .await?;
        }
        Ok(if cancelled {
            Tools::Cancelled
        } else if limited {
            Tools::Limit
        } else {
            Tools::Continue
        })
    }

    /// Stores an assistant message that names a limit, and ends the turn as a limit.
    async fn limit(&mut self, text: &str) -> Outcome {
        let stored = self
            .store(Stored {
                role: MessageRole::Assistant,
                parts: vec![AssistantPart::Text {
                    content: text.to_owned(),
                }],
                status: MessageStatus::Complete,
                stubs: false,
            })
            .await;
        match stored {
            Ok(()) => Outcome::Limit,
            Err(outcome) => outcome,
        }
    }

    /// Appends one message to the conversation and to `rows`. A full conversation and any storage
    /// failure are errors; neither carries the store's text.
    async fn store(&mut self, message: Stored) -> Result<(), Outcome> {
        let parts = serde_json::to_string(&message.parts).map_err(|_| internal())?;
        let now = self.state.clock.now_ns();
        let stub_after = message.stubs.then(|| now.saturating_add(STUB_AFTER_NS));
        let conversation = self.setup.conversation;
        let row_parts = parts.clone();
        let new = NewMessage {
            role: message.role,
            parts,
            context: None,
            status: message.status,
            turn_trace_id: Some(self.trace_hex.clone()),
            stub_after,
        };
        let written = self
            .state
            .meta
            .write(move |conn| {
                Box::pin(async move { append_message(conn, conversation, new, now).await })
            })
            .await;
        match written {
            Ok(seq) => {
                self.rows.push(MessageRow {
                    seq,
                    role: message.role,
                    parts: row_parts,
                    context: None,
                    status: message.status,
                    feedback: None,
                    turn_trace_id: Some(self.trace_hex.clone()),
                    created_at: now,
                });
                Ok(())
            }
            Err(MetaError::Conflict(conflict)) if conflict == TOO_MANY_MESSAGES => {
                Err(Outcome::error(PERMANENT, TOO_MANY_MESSAGES))
            }
            Err(_) => {
                tracing::error!(
                    conversation_id = %conversation,
                    trace_id = %self.trace_hex,
                    "assistant turn could not store a message"
                );
                Err(internal())
            }
        }
    }

    /// Traces the turn, commits the trace, and sends the last event unless the client left.
    async fn finish(&self, outcome: Outcome) {
        let ended_ns = self.state.clock.now_ns();
        let error = match &outcome {
            Outcome::Error { message, .. } => Some(message.clone()),
            Outcome::Cancelled => Some(CANCELLED.to_owned()),
            Outcome::Done(_) | Outcome::Limit => None,
        };
        let trace = AssistantTurnTrace {
            trace_id: self.setup.trace_id,
            started_ns: self.started_ns,
            ended_ns,
            adapter: self.setup.connection.adapter,
            model: &self.setup.model,
            calls: &self.model_calls,
            tools: &self.tool_calls,
            error: error.as_deref(),
            user: self.setup.user.id,
            conversation: self.setup.conversation,
            prompt_version: PROMPT_VERSION,
        };
        let ingest = self
            .state
            .ingest
            .context(&self.setup.project, None, ended_ns);
        let prices = self
            .state
            .ingest
            .prices()
            .for_project(self.setup.project.org_id, self.setup.project.id);
        let normalized = build_assistant_trace(&trace, &ingest, &prices);
        // The cost of the turn, read before the spans move into the commit.
        let cost_usd = turn_cost(
            normalized
                .spans
                .iter()
                .filter(|span| span.span_kind == SpanKind::Llm)
                .map(|span| {
                    let has_usage = span.input_tokens.is_some() || span.output_tokens.is_some();
                    (has_usage, span.cost_total)
                }),
        );
        if commit_judge_trace(self.state, &self.setup.project, ended_ns, normalized)
            .await
            .is_err()
        {
            tracing::warn!(
                project_id = %self.setup.project.id,
                trace_id = %self.trace_hex,
                "could not commit the assistant turn trace"
            );
        }

        let (outcome_name, event) = match outcome {
            Outcome::Done(finish_reason) => (
                "done",
                Some(TurnEvent::Done {
                    finish_reason,
                    usage: self.usage,
                    cost_usd,
                    steps: self.model_calls.len(),
                }),
            ),
            Outcome::Limit => (
                "limit",
                Some(TurnEvent::Done {
                    finish_reason: "limit".to_owned(),
                    usage: self.usage,
                    cost_usd,
                    steps: self.model_calls.len(),
                }),
            ),
            Outcome::Error { class, message } => (
                "error",
                Some(TurnEvent::Error {
                    class: class.to_owned(),
                    message,
                }),
            ),
            Outcome::Cancelled => ("cancelled", None),
        };
        if let Some(event) = event {
            let _ = self.emit(event).await;
        }
        tracing::info!(
            target: "audit",
            project_id = %self.setup.project.id,
            conversation_id = %self.setup.conversation,
            user_id = %self.setup.user.id,
            trace_id = %self.trace_hex,
            outcome = outcome_name,
            steps = self.model_calls.len(),
            tool_calls = self.tool_calls.len(),
            "assistant turn"
        );
    }
}

/// The system prompt with the org and project, the time and the page context (D-AS-13, D-AS-14).
fn system_message(setup: &TurnSetup, now_ns: i64) -> String {
    let mut system = SYSTEM_PROMPT
        .replace("{org}", &setup.org_slug)
        .replace("{project}", &setup.project.slug);
    let now = chrono::DateTime::from_timestamp_nanos(now_ns)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    system.push_str(&format!("\n\nProject: {}. Now: {now}.", setup.project.name));
    if let Some(context) = &setup.context {
        system.push_str(&format!(
            "\n\nPage context (sent by the UI; may be stale):\n{context}"
        ));
    }
    system
}

/// The tools as the provider sees them.
fn definitions(specs: Vec<ToolSpec>) -> Vec<ToolDefinition> {
    specs
        .into_iter()
        .map(|spec| ToolDefinition {
            name: spec.name.to_owned(),
            description: Some(spec.description.to_owned()),
            parameters: spec.parameters,
        })
        .collect()
}

/// The parts of an assistant message: its text when not empty, then its tool calls in order.
fn assistant_parts(text: &str, calls: &[ToolCall]) -> Vec<AssistantPart> {
    let mut parts = Vec::with_capacity(calls.len() + 1);
    if !text.is_empty() {
        parts.push(AssistantPart::Text {
            content: text.to_owned(),
        });
    }
    parts.extend(calls.iter().map(|call| AssistantPart::ToolCall {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
    }));
    parts
}

/// A model call's record with no tokens, for a call that never reached the provider.
fn bare_record(started_ns: i64, ended_ns: i64, error: impl Into<String>) -> ModelCallRecord {
    ModelCallRecord {
        started_ns,
        ended_ns,
        response_model: None,
        usage: None,
        finish_reason: None,
        ttft_ns: None,
        error: Some(error.into()),
    }
}

/// A result that says the call did not run. Its error has the shape of the other tool errors, so
/// the summary and the trace read its `not_run` code.
fn not_run(message: &str) -> ToolResult {
    ToolResult {
        ok: false,
        value: json!({"error": {"code": "not_run", "message": message}}),
        rows: None,
        draft: None,
    }
}

/// Runs one tool call. Non-object arguments and unknown names are tool errors, and the turn goes on.
async fn run_tool_call(state: &AppState, scope: &ToolScope, call: &ToolCall) -> ToolResult {
    let Value::Object(arguments) = &call.arguments else {
        return ToolResult {
            ok: false,
            value: json!({"error": {"code": "bad_request", "message": "arguments must be a JSON object"}}),
            rows: None,
            draft: None,
        };
    };
    match tools::run(state, scope, &call.name, arguments.clone()).await {
        Ok(result) => result,
        Err(UnknownTool) => ToolResult {
            ok: false,
            value: json!({"error": {"code": "not_found", "message": "unknown tool"}}),
            rows: None,
            draft: None,
        },
    }
}

/// The tool name the trace records: the assistant's own name, or `unknown` for a name the model
/// made up, so that no model output reaches the trace (D-AS-11).
fn trace_tool_name(name: &str) -> &'static str {
    specs()
        .iter()
        .find(|spec| spec.name == name)
        .map_or("unknown", |spec| spec.name)
}

/// The turn's cost (D-AS-11), from each model call as (has usage, cost): the sum of the costs of
/// the calls that have usage, when every one of them has a cost. `None` otherwise, and when no
/// call has usage.
fn turn_cost(calls: impl Iterator<Item = (bool, Option<f64>)>) -> Option<f64> {
    let mut total = 0.0;
    let mut any_usage = false;
    for (has_usage, cost) in calls {
        if !has_usage {
            continue;
        }
        any_usage = true;
        total += cost?;
    }
    any_usage.then_some(total)
}

/// The class of a store error for the log: its variant, never its text.
fn meta_error_class(error: &MetaError) -> &'static str {
    match error {
        MetaError::Open { .. } => "open",
        MetaError::Migrate { .. } => "migrate",
        MetaError::NotInitialized => "not_initialized",
        MetaError::PreMigrationIo(_) | MetaError::PreMigrationDb(_) => "pre_migration",
        MetaError::SchemaMismatch { .. } => "schema_mismatch",
        MetaError::Db(_) => "db",
        MetaError::Conflict(_) => "conflict",
        MetaError::Corrupt { .. } => "corrupt",
    }
}

/// The error class names the events and the trace use (`ErrorClass`).
fn class_name(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Retryable => RETRYABLE,
        ErrorClass::Pause => "pause",
        ErrorClass::Permanent => PERMANENT,
    }
}

fn internal() -> Outcome {
    Outcome::error(PERMANENT, INTERNAL)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::{Method, header};
    use cetra_capture::{Mock, Turn as MockTurn};
    use cetra_core::message::Part;
    use serde_json::{Value, json};
    use tokio::sync::mpsc;

    use super::*;
    use crate::assistant::history;
    use crate::auth::limits::AssistantLimits;
    use crate::eval::interactive;
    use crate::mcp::test_helpers::{FROM, TO};
    use crate::meta::assistant::insert_conversation;
    use crate::meta::connections;
    use crate::test_support::{Seeded, TestApp, capture_logs};

    fn real_now() -> i64 {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after the epoch")
                .as_nanos(),
        )
        .expect("fits i64")
    }

    /// A seeded app on the real clock, with private egress allowed for the mock and the example
    /// dataset loaded.
    async fn new_app() -> (TestApp, Seeded) {
        let app = TestApp::with_config(|config| {
            config.security.allow_private_network_egress = true;
        })
        .await;
        app.state.clock.set(real_now());
        let seeded = app.seed().await;
        app.load_example_dataset(seeded.project.id).await;
        (app, seeded)
    }

    /// Serves `turns` on loopback as an OpenAI-style provider. Returns the base URL.
    async fn provider(turns: Vec<MockTurn>) -> String {
        let mock = Mock::new(turns);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("address");
        let router = mock.router();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}/v1")
    }

    #[allow(clippy::needless_pass_by_value)] // `json!` reads the value by reference.
    fn turn(message: Value, finish: &str, chunks: u32, ttft_ms: u64) -> MockTurn {
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": message,
            "finish": finish,
            "usage": {"input": 10, "output": 3},
            "chunks": chunks,
            "ttft_ms": ttft_ms,
            "chunk_delay_ms": 0,
        }))
        .expect("a turn")
    }

    fn text_turn(text: &str, chunks: u32) -> MockTurn {
        turn(json!({"text": text}), "stop", chunks, 0)
    }

    /// A text answer of `chunks` pieces, each sent `chunk_delay_ms` after the last.
    fn slow_text_turn(text: &str, chunks: u32, chunk_delay_ms: u64) -> MockTurn {
        serde_json::from_value(json!({
            "api": "openai.chat",
            "model": "gpt-test",
            "message": {"text": text},
            "finish": "stop",
            "usage": {"input": 10, "output": 3},
            "chunks": chunks,
            "ttft_ms": 0,
            "chunk_delay_ms": chunk_delay_ms,
        }))
        .expect("a turn")
    }

    #[allow(clippy::needless_pass_by_value)] // `json!` reads the value by reference.
    fn calls_turn(calls: Value) -> MockTurn {
        turn(json!({"tool_calls": calls}), "tool_calls", 1, 0)
    }

    #[allow(clippy::needless_pass_by_value)] // `json!` reads the value by reference.
    fn call(id: &str, name: &str, arguments: Value) -> Value {
        json!({"id": id, "name": name, "arguments": arguments})
    }

    /// Creates a connection to `base` in `acme` and returns its id.
    async fn connection(app: &TestApp, seeded: &Seeded, base: &str, max_concurrency: u32) -> Uuid {
        let response = app
            .request(Method::POST, "/api/v1/orgs/acme/llm-connections")
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .json(&json!({
                "name": format!("Mocked {}", Uuid::now_v7()),
                "adapter": "openai",
                "api_key": "sk-test-key-0123456789",
                "base_url": base,
                "default_model": "gpt-test",
                "max_concurrency": max_concurrency,
                "requests_per_minute": 300,
                "structured_output": "native",
            }))
            .send()
            .await;
        assert_eq!(
            response.status,
            201,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        let value: Value = response.json();
        Uuid::parse_str(value["id"].as_str().expect("id")).expect("uuid")
    }

    async fn connection_row(app: &TestApp, seeded: &Seeded, id: Uuid) -> ConnectionRow {
        connections::get_connection(app.state.meta.reader(), seeded.org_id, id)
            .await
            .expect("the query")
            .expect("the connection")
    }

    async fn new_conversation(app: &TestApp, seeded: &Seeded) -> Uuid {
        let (project, user, now) = (seeded.project.id, seeded.user_id, app.state.clock.now_ns());
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    let row = insert_conversation(conn, Uuid::now_v7(), project, user, now).await?;
                    Ok::<Uuid, MetaError>(row.id)
                })
            })
            .await
            .expect("a conversation")
    }

    /// Stores a user message as the route does, and returns its sequence number.
    async fn add_user_message(app: &TestApp, conversation: Uuid, text: &str) -> i64 {
        let now = app.state.clock.now_ns();
        let parts = json!([{"type": "text", "content": text}]).to_string();
        app.state
            .meta
            .write(move |conn| {
                Box::pin(async move {
                    append_message(
                        conn,
                        conversation,
                        NewMessage {
                            role: MessageRole::User,
                            parts,
                            context: None,
                            status: MessageStatus::Complete,
                            turn_trace_id: None,
                            stub_after: None,
                        },
                        now,
                    )
                    .await
                })
            })
            .await
            .expect("the user message")
    }

    /// The turn's setup for `connection`, in a new conversation whose first message is `text`.
    async fn turn_setup(
        app: &TestApp,
        seeded: &Seeded,
        connection: Uuid,
        conversation: Uuid,
        user_seq: i64,
        trace_id: TraceId,
    ) -> TurnSetup {
        let row = connection_row(app, seeded, connection).await;
        let judge = app
            .state
            .connections
            .get(&app.state.secrets, &row)
            .expect("the connection opens");
        TurnSetup {
            project: seeded.project.clone(),
            org_slug: "acme".to_owned(),
            user: AuthUser {
                id: seeded.user_id,
                is_instance_admin: false,
                email: "ada@example.com".to_owned(),
            },
            conversation,
            user_seq,
            context: None,
            connection_row: row,
            connection: judge,
            model: "gpt-test".to_owned(),
            trace_id,
            permit_wait: Duration::from_secs(1),
            turn_deadline: TURN_DEADLINE,
        }
    }

    fn guard(seeded: &Seeded, conversation: Uuid) -> AssistantGuard {
        AssistantLimits::new()
            .admit(conversation, seeded.user_id, seeded.org_id, real_now())
            .expect("the turn is admitted")
    }

    fn trace(n: u8) -> TraceId {
        TraceId::from_bytes([n; 16]).expect("a valid trace id")
    }

    /// Runs one turn to its end and collects its events.
    async fn drive(app: &TestApp, setup: TurnSetup, seeded: &Seeded) -> Vec<TurnEvent> {
        let conversation = setup.conversation;
        let (tx, mut rx) = mpsc::channel(256);
        tokio::spawn(run_turn(
            app.state.clone(),
            setup,
            tx,
            guard(seeded, conversation),
        ));
        let mut events = Vec::new();
        while let Some(event) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("the turn ends")
        {
            events.push(event);
        }
        events
    }

    fn names(events: &[TurnEvent]) -> Vec<&'static str> {
        events.iter().map(TurnEvent::name).collect()
    }

    /// The stored messages of the conversation, in sequence order.
    async fn stored(app: &TestApp, seeded: &Seeded, conversation: Uuid) -> Vec<MessageRow> {
        get_conversation_with_messages(
            app.state.meta.reader(),
            seeded.project.id,
            seeded.user_id,
            conversation,
        )
        .await
        .expect("the query")
        .expect("the conversation")
        .1
    }

    fn parts_of(row: &MessageRow) -> Vec<AssistantPart> {
        serde_json::from_str(&row.parts).expect("stored parts")
    }

    /// The trace's detail as the API answers it.
    async fn trace_detail(app: &TestApp, seeded: &Seeded, trace_hex: &str) -> Value {
        let path = format!("/api/v1/projects/{}/traces/{trace_hex}", seeded.project.id);
        let response = app
            .request(Method::GET, &path)
            .header(header::COOKIE.as_str(), &seeded.cookie)
            .send()
            .await;
        assert_eq!(
            response.status,
            200,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
        response.json()
    }

    fn span_count(detail: &Value) -> usize {
        detail["spans"].as_array().map_or(0, Vec::len)
    }

    /// The units counted for the project, from `usage_daily`.
    async fn units(app: &TestApp) -> i64 {
        sqlx::query_scalar("SELECT COALESCE(SUM(spans), 0) FROM usage_daily")
            .fetch_one(app.state.meta.reader())
            .await
            .expect("the units")
    }

    /// The stub time of a stored message (`assistant_messages.stub_after`).
    async fn stub_after(app: &TestApp, conversation: Uuid, seq: i64) -> Option<i64> {
        sqlx::query_scalar(
            "SELECT stub_after FROM assistant_messages WHERE conversation_id = ? AND seq = ?",
        )
        .bind(conversation.to_string())
        .bind(seq)
        .fetch_one(app.state.meta.reader())
        .await
        .expect("the message")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn answers_without_tools() {
        let base = provider(vec![text_turn("One two three.", 3)]).await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Count to three.").await;
        let units_before = units(&app).await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(1)).await;
        let events = drive(&app, setup, &seeded).await;
        assert_eq!(names(&events), ["start", "text", "text", "text", "done"]);
        assert!(matches!(
            events.last(),
            Some(TurnEvent::Done { finish_reason, steps: 1, .. }) if finish_reason == "stop"
        ));

        let rows = stored(&app, &seeded, conversation).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].role, MessageRole::Assistant);
        assert_eq!(rows[1].status, MessageStatus::Complete);
        assert_eq!(
            parts_of(&rows[1]),
            [AssistantPart::Text {
                content: "One two three.".to_owned()
            }]
        );

        let detail = trace_detail(&app, &seeded, &trace(1).to_hex()).await;
        assert_eq!(span_count(&detail), 2, "the root and the model call");
        assert_eq!(
            units(&app).await,
            units_before,
            "a turn is not counted as units"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runs_a_tool_then_answers() {
        let base = provider(vec![
            calls_turn(json!([call(
                "call_1",
                "query_sql",
                json!({"query": "SELECT count(*) AS n FROM traces", "from": FROM, "to": TO}),
            )])),
            text_turn("There are traces.", 1),
        ])
        .await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "How many traces?").await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(2)).await;
        let events = drive(&app, setup, &seeded).await;
        assert!(events.iter().any(|event| matches!(
            event,
            TurnEvent::ToolCall { name, .. } if name == "query_sql"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            TurnEvent::ToolResult { ok: true, summary, .. } if summary.rows == Some(1)
        )));
        assert!(matches!(events.last(), Some(TurnEvent::Done { .. })));

        let rows = stored(&app, &seeded, conversation).await;
        let roles: Vec<MessageRole> = rows.iter().map(|row| row.role).collect();
        assert_eq!(
            roles,
            [
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::Tool,
                MessageRole::Assistant
            ]
        );
        let stub = stub_after(&app, conversation, rows[2].seq).await;
        assert_eq!(stub, Some(rows[2].created_at + STUB_AFTER_NS));

        let detail = trace_detail(&app, &seeded, &trace(2).to_hex()).await;
        assert_eq!(span_count(&detail), 4, "root, two model calls, one tool");
        // No message text, SQL or answer is in the trace (D-AS-11).
        let text = detail.to_string();
        assert!(!text.contains("SELECT count(*)"), "{text}");
        assert!(!text.contains("There are traces."), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn drafts_are_streamed_and_stored() {
        let base = provider(vec![
            calls_turn(json!([call(
                "call_1",
                "draft_filter",
                json!({"view": "traces", "filter": "status = 'error'"}),
            )])),
            text_turn("Apply the draft.", 1),
        ])
        .await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Show errors.").await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(3)).await;
        let events = drive(&app, setup, &seeded).await;
        assert!(events.iter().any(|event| matches!(
            event,
            TurnEvent::Draft { id, kind: AssistantDraftKind::Filter, .. } if id == "draft_call_1"
        )));

        let rows = stored(&app, &seeded, conversation).await;
        let tool_parts = parts_of(&rows[2]);
        let result_at = tool_parts
            .iter()
            .position(|part| matches!(part, AssistantPart::ToolResult { id, .. } if id == "call_1"))
            .expect("the result");
        assert!(matches!(
            tool_parts.get(result_at + 1),
            Some(AssistantPart::Draft { id, .. }) if id == "draft_call_1"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_tool_is_a_tool_error() {
        let base = provider(vec![
            calls_turn(json!([call("call_1", "nope", json!({}))])),
            text_turn("Sorry.", 1),
        ])
        .await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Use a tool.").await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(4)).await;
        let events = drive(&app, setup, &seeded).await;
        assert!(events.iter().any(|event| matches!(
            event,
            TurnEvent::ToolResult { ok: false, summary, .. }
                if summary.error_code.as_deref() == Some("not_found")
        )));
        assert!(matches!(events.last(), Some(TurnEvent::Done { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_tool_call_refuses_non_object_arguments() {
        let (app, seeded) = new_app().await;
        let call = ToolCall {
            id: "call_1".to_owned(),
            name: "get_project_settings".to_owned(),
            arguments: Value::String("x".to_owned()),
        };
        let scope = ToolScope {
            user: AuthUser {
                id: seeded.user_id,
                is_instance_admin: false,
                email: "ada@example.com".to_owned(),
            },
            project: seeded.project.id,
        };
        let result = run_tool_call(&app.state, &scope, &call).await;
        assert!(!result.ok);
        assert_eq!(result.value["error"]["code"], "bad_request");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stops_at_thirty_two_tool_calls() {
        let calls: Vec<Value> = (0..33)
            .map(|index| call(&format!("call_{index}"), "get_project_settings", json!({})))
            .collect();
        let base = provider(vec![calls_turn(Value::Array(calls))]).await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Read everything.").await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(5)).await;
        let events = drive(&app, setup, &seeded).await;
        let results: Vec<bool> = events
            .iter()
            .filter_map(|event| match event {
                TurnEvent::ToolResult { ok, .. } => Some(*ok),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 33);
        assert_eq!(results.iter().filter(|ok| **ok).count(), 32);
        assert!(!results[32], "the 33rd call does not run");
        assert!(matches!(
            events.last(),
            Some(TurnEvent::Done { finish_reason, .. }) if finish_reason == "limit"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stops_at_sixteen_model_calls() {
        let turns: Vec<MockTurn> = (0..16)
            .map(|index| {
                calls_turn(json!([call(
                    &format!("call_{index}"),
                    "get_project_settings",
                    json!({}),
                )]))
            })
            .collect();
        let base = provider(turns).await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Keep looking.").await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(6)).await;
        let events = drive(&app, setup, &seeded).await;
        assert!(matches!(
            events.last(),
            Some(TurnEvent::Done { finish_reason, steps: 16, .. }) if finish_reason == "limit"
        ));
        let rows = stored(&app, &seeded, conversation).await;
        let last = rows.last().expect("a last message");
        let text = match parts_of(last).first() {
            Some(AssistantPart::Text { content }) => content.clone(),
            _ => String::new(),
        };
        assert!(text.contains("16 model calls"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn time_limit_stores_the_limit_message() {
        // Thirty chunks 100 ms apart run past the 300 ms deadline. The call's whole-call timeout is
        // at least a second (MIN_CALL_TIMEOUT), so the timeout ends the call with part of the answer.
        let base = provider(vec![slow_text_turn(
            "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen",
            30,
            100,
        )])
        .await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Count slowly.").await;

        let mut setup =
            turn_setup(&app, &seeded, connection, conversation, user_seq, trace(16)).await;
        setup.turn_deadline = Duration::from_millis(300);
        let events = drive(&app, setup, &seeded).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TurnEvent::Text { .. })),
            "the partial answer streamed"
        );
        assert!(matches!(
            events.last(),
            Some(TurnEvent::Done { finish_reason, steps: 1, .. }) if finish_reason == "limit"
        ));

        let rows = stored(&app, &seeded, conversation).await;
        assert_eq!(
            rows.len(),
            3,
            "the user message, the partial answer, the limit"
        );
        assert_eq!(rows[1].role, MessageRole::Assistant);
        assert_eq!(rows[1].status, MessageStatus::Cancelled);
        assert!(
            matches!(parts_of(&rows[1]).first(), Some(AssistantPart::Text { content }) if !content.is_empty()),
            "the partial answer holds its text"
        );
        assert_eq!(rows[2].role, MessageRole::Assistant);
        assert_eq!(rows[2].status, MessageStatus::Complete);
        let text = match parts_of(&rows[2]).first() {
            Some(AssistantPart::Text { content }) => content.clone(),
            _ => String::new(),
        };
        assert!(text.contains("limit of 10 minutes"), "{text}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_tool_name_is_not_traced() {
        const CANARY: &str = "canary_tool_name_7f3a";
        let base = provider(vec![
            calls_turn(json!([call("call_1", CANARY, json!({}))])),
            text_turn("Sorry.", 1),
        ])
        .await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Use a tool.").await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(18)).await;
        let events = drive(&app, setup, &seeded).await;
        assert!(matches!(events.last(), Some(TurnEvent::Done { .. })));

        let detail = trace_detail(&app, &seeded, &trace(18).to_hex()).await;
        let names: Vec<&str> = detail["spans"]
            .as_array()
            .expect("the spans")
            .iter()
            .filter_map(|span| span["span"]["name"].as_str())
            .collect();
        assert!(names.contains(&"tool unknown"), "{names:?}");
        assert!(!detail.to_string().contains(CANARY), "{detail}");
    }

    #[test]
    fn turn_cost_needs_a_cost_for_every_call_with_usage() {
        let cost = |calls: &[(bool, Option<f64>)]| turn_cost(calls.iter().copied());
        assert_eq!(
            cost(&[(true, Some(0.5)), (false, None), (true, Some(1.0))]),
            Some(1.5)
        );
        assert_eq!(cost(&[(true, Some(0.5)), (true, None)]), None);
        assert_eq!(cost(&[(false, None)]), None, "no call reported usage");
        assert_eq!(cost(&[]), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn length_finish_ends_the_turn() {
        let base = provider(vec![turn(json!({"text": "Cut off"}), "length", 1, 0)]).await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Long answer.").await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(7)).await;
        let events = drive(&app, setup, &seeded).await;
        assert!(matches!(
            events.last(),
            Some(TurnEvent::Done { finish_reason, .. }) if finish_reason == "length"
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_answer_is_stored_empty() {
        let base = provider(vec![turn(json!({}), "stop", 1, 0)]).await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Say nothing.").await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(8)).await;
        let events = drive(&app, setup, &seeded).await;
        assert!(matches!(events.last(), Some(TurnEvent::Done { .. })));
        let rows = stored(&app, &seeded, conversation).await;
        assert_eq!(rows.len(), 2);
        assert!(parts_of(&rows[1]).is_empty());
        assert_eq!(rows[1].status, MessageStatus::Complete);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_around_tools() {
        let base = provider(vec![calls_turn(json!([
            call("call_1", "get_project_settings", json!({})),
            call("call_2", "get_project_settings", json!({})),
            call("call_3", "get_project_settings", json!({})),
        ]))])
        .await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Read three.").await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(9)).await;
        // One slot: the turn waits for each event to be read. After the three tool-call events are
        // read, the turn runs the first tool and waits to send its result, so the third never runs.
        let (tx, mut rx) = mpsc::channel(1);
        let task = tokio::spawn(run_turn(
            app.state.clone(),
            setup,
            tx,
            guard(&seeded, conversation),
        ));
        let mut tool_call_events = 0;
        while tool_call_events < 3 {
            match rx.recv().await {
                Some(TurnEvent::ToolCall { .. }) => tool_call_events += 1,
                Some(_) => {}
                None => panic!("the turn ended before its tool calls"),
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(rx);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the turn ends within 2 s")
            .expect("the turn does not panic");

        // The tool message holds results only for the calls that ran, in call order. The third
        // never ran, so at most two results are stored.
        let expected = ["call_1", "call_2", "call_3"];
        let rows = stored(&app, &seeded, conversation).await;
        let stored_ids: Vec<String> = rows
            .iter()
            .filter(|row| row.role == MessageRole::Tool)
            .flat_map(parts_of)
            .filter_map(|part| match part {
                AssistantPart::ToolResult { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        assert!(stored_ids.len() <= 2, "{stored_ids:?}");
        let ids: Vec<&str> = stored_ids.iter().map(String::as_str).collect();
        assert_eq!(ids, expected[..stored_ids.len()].to_vec());

        // The history answers each of the three calls. A call with no stored result gets the
        // not-run error, in the shape of the other tool errors.
        let sent = history::provider_messages(SYSTEM_PROMPT, &rows).expect("the history builds");
        let answers: Vec<(String, Value)> = sent
            .iter()
            .flat_map(|message| message.parts.iter())
            .filter_map(|part| match part {
                Part::ToolCallResponse {
                    id: Some(id),
                    response,
                    ..
                } => Some((id.clone(), response.clone())),
                _ => None,
            })
            .collect();
        let answered: Vec<&str> = answers.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(answered, expected);
        for (id, response) in &answers {
            if stored_ids.contains(id) {
                assert_ne!(response["error"]["code"], "not_run", "{id}");
            } else {
                assert_eq!(response["error"]["code"], "not_run", "{id}");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_stores_a_cancelled_message() {
        let base = provider(vec![turn(json!({"text": "Late."}), "stop", 1, 5000)]).await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Wait.").await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(10)).await;
        let (tx, mut rx) = mpsc::channel(256);
        let task = tokio::spawn(run_turn(
            app.state.clone(),
            setup,
            tx,
            guard(&seeded, conversation),
        ));
        let first = rx.recv().await;
        assert!(matches!(first, Some(TurnEvent::Start { .. })));
        drop(rx);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let rows = stored(&app, &seeded, conversation).await;
            let cancelled = rows.iter().any(|row| {
                row.role == MessageRole::Assistant && row.status == MessageStatus::Cancelled
            });
            // The trace is committed after the message is stored, so it may not exist yet.
            let response = app
                .request(
                    Method::GET,
                    &format!(
                        "/api/v1/projects/{}/traces/{}",
                        seeded.project.id,
                        trace(10).to_hex()
                    ),
                )
                .header(header::COOKIE.as_str(), &seeded.cookie)
                .send()
                .await;
            let errored =
                response.status == 200 && response.json::<Value>()["trace"]["status"] == "error";
            if cancelled && errored {
                break;
            }
            assert!(Instant::now() < deadline, "the cancel was not stored");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the task ends")
            .expect("no panic");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn busy_connection_ends_the_turn() {
        let base = provider(vec![]).await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 1).await;
        let row = connection_row(&app, &seeded, connection).await;
        let _held = interactive::acquire(&app.state, &row, Duration::from_secs(1))
            .await
            .expect("the only slot");
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "Busy?").await;

        let mut setup =
            turn_setup(&app, &seeded, connection, conversation, user_seq, trace(11)).await;
        setup.permit_wait = Duration::from_millis(200);
        let events = drive(&app, setup, &seeded).await;
        assert!(matches!(
            events.last(),
            Some(TurnEvent::Error { class, message })
                if class == "retryable" && message == BUSY
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_turn_answers_over_the_stored_history() {
        let base = provider(vec![text_turn("First.", 1), text_turn("Second.", 1)]).await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let first_seq = add_user_message(&app, conversation, "Hello.").await;
        let setup = turn_setup(
            &app,
            &seeded,
            connection,
            conversation,
            first_seq,
            trace(12),
        )
        .await;
        assert!(matches!(
            drive(&app, setup, &seeded).await.last(),
            Some(TurnEvent::Done { .. })
        ));

        let second_seq = add_user_message(&app, conversation, "And again?").await;
        let setup = turn_setup(
            &app,
            &seeded,
            connection,
            conversation,
            second_seq,
            trace(13),
        )
        .await;
        assert!(matches!(
            drive(&app, setup, &seeded).await.last(),
            Some(TurnEvent::Done { .. })
        ));

        // The second turn sent the system message and the three messages before its answer.
        let rows = stored(&app, &seeded, conversation).await;
        assert_eq!(rows.len(), 4);
        let sent = history::provider_messages(SYSTEM_PROMPT, &rows[..3]).expect("the history");
        assert_eq!(sent.len(), 4);
    }

    // One thread: the log capture is a thread-local subscriber.
    #[tokio::test]
    async fn audit_log_has_no_payload() {
        let base = provider(vec![text_turn("Canary answer.", 1)]).await;
        let (app, seeded) = new_app().await;
        let connection = connection(&app, &seeded, &base, 4).await;
        let conversation = new_conversation(&app, &seeded).await;
        let user_seq = add_user_message(&app, conversation, "canary-question").await;

        let setup = turn_setup(&app, &seeded, connection, conversation, user_seq, trace(14)).await;
        let (events, logs) = capture_logs(async { drive(&app, setup, &seeded).await }).await;
        assert!(matches!(events.last(), Some(TurnEvent::Done { .. })));
        assert!(logs.contains("assistant turn"), "{logs}");
        assert!(!logs.contains("canary-question"), "{logs}");
        assert!(!logs.contains("Canary answer."), "{logs}");
    }
}
