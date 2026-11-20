//! A streamed call: the SSE events of one response, decoded into [`ChatEvent`]s.

use std::collections::VecDeque;
use std::fmt;

use super::sse::{SseDecoder, SseEvent};
use super::{ANSWER_MAX_BYTES, ChatDone, ChatEvent, ToolCall, anthropic, gemini, openai};
use crate::egress::EgressStream;
use crate::error::{ChatError, EgressError};

/// Events the decoder produced and the answer's size so far (D-V-30).
#[derive(Default)]
pub(crate) struct Sink {
    events: VecDeque<ChatEvent>,
    answer_bytes: usize,
}

impl Sink {
    /// Queues a text piece. Empty text is dropped.
    pub(crate) fn text(&mut self, text: &str) -> Result<(), ChatError> {
        if text.is_empty() {
            return Ok(());
        }
        self.count(text.len())?;
        self.events.push_back(ChatEvent::Text(text.to_owned()));
        Ok(())
    }

    /// Queues a complete tool call. The name and the compact JSON of its arguments count
    /// toward the cap.
    pub(crate) fn tool_call(&mut self, call: ToolCall) -> Result<(), ChatError> {
        let arguments = serde_json::to_string(&call.arguments).map_or(0, |text| text.len());
        self.count(call.name.len().saturating_add(arguments))?;
        self.events.push_back(ChatEvent::ToolCall(call));
        Ok(())
    }

    /// Queues the last event.
    pub(crate) fn done(&mut self, done: ChatDone) {
        self.events.push_back(ChatEvent::Done(done));
    }

    fn count(&mut self, bytes: usize) -> Result<(), ChatError> {
        self.answer_bytes = self.answer_bytes.saturating_add(bytes);
        if self.answer_bytes > ANSWER_MAX_BYTES {
            return Err(ChatError::Transport(EgressError::TooLarge {
                limit: ANSWER_MAX_BYTES,
            }));
        }
        Ok(())
    }
}

/// Whether the decoder saw the provider's end of the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// Keep reading events.
    Continue,
    /// The answer is complete; the rest of the body is not read.
    End,
}

/// The provider's decoder for one streamed answer.
pub(crate) enum Decoder {
    OpenAi(openai::StreamState),
    Anthropic(anthropic::StreamState),
    Gemini(gemini::StreamState),
}

impl Decoder {
    fn on_event(&mut self, event: &SseEvent, sink: &mut Sink) -> Result<Flow, ChatError> {
        // An empty `data:` event is a keep-alive. No provider sends one with content.
        if event.data.trim().is_empty() {
            return Ok(Flow::Continue);
        }
        match self {
            Self::OpenAi(state) => state.on_event(event, sink),
            Self::Anthropic(state) => state.on_event(event, sink),
            Self::Gemini(state) => state.on_event(event, sink),
        }
    }

    fn finish(&mut self, sink: &mut Sink) -> Result<(), ChatError> {
        match self {
            Self::OpenAi(state) => state.finish(sink),
            Self::Anthropic(state) => state.finish(sink),
            Self::Gemini(state) => {
                state.finish(sink);
                Ok(())
            }
        }
    }

    /// The body ended before the provider's end signal: a stream cut short is not an answer.
    /// Anthropic's end signal `message_stop` returns [`Flow::End`] before the body ends, so
    /// a body that reaches its end without it is always cut short.
    fn finish_at_eof(&mut self, sink: &mut Sink) -> Result<(), ChatError> {
        let ended = match self {
            Self::OpenAi(state) => state.ended(),
            Self::Anthropic(_) => false,
            Self::Gemini(state) => state.ended(),
        };
        if !ended {
            return Err(ChatError::BadStream);
        }
        self.finish(sink)
    }
}

/// A streamed chat call. Dropping it closes the provider connection.
pub struct ChatStream {
    body: EgressStream,
    sse: SseDecoder,
    decoder: Decoder,
    sink: Sink,
    finished: bool,
    error: Option<ChatError>,
}

impl fmt::Debug for ChatStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChatStream")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl ChatStream {
    /// A stream that reads `body` with `decoder`.
    pub(crate) fn new(body: EgressStream, decoder: Decoder) -> Self {
        Self {
            body,
            sse: SseDecoder::new(ANSWER_MAX_BYTES),
            decoder,
            sink: Sink::default(),
            finished: false,
            error: None,
        }
    }

    /// The next event, or `None` after [`ChatEvent::Done`] or after an error.
    ///
    /// Events read before an error are returned first.
    ///
    /// # Errors
    ///
    /// [`ChatError::Transport`] for a timeout (the whole call, or a silence longer than
    /// the idle timeout), a failed connection, or a passed cap; [`ChatError::Provider`]
    /// for an error event; [`ChatError::BadStream`] for an event that cannot be read or
    /// a body that ends before the provider's end.
    pub async fn next(&mut self) -> Result<Option<ChatEvent>, ChatError> {
        loop {
            if let Some(event) = self.sink.events.pop_front() {
                return Ok(Some(event));
            }
            if let Some(error) = self.error.take() {
                return Err(error);
            }
            if self.finished {
                return Ok(None);
            }
            if let Err(error) = self.advance().await {
                self.finished = true;
                self.error = Some(error);
            }
        }
    }

    /// Reads one body chunk, or the end of the body, and decodes what it completes.
    async fn advance(&mut self) -> Result<(), ChatError> {
        let Some(chunk) = self.body.next_chunk().await? else {
            let events = self.sse.finish()?;
            if !self.feed(&events)? {
                if let Some(error) = self.sse.take_error() {
                    return Err(error);
                }
                self.decoder.finish_at_eof(&mut self.sink)?;
            }
            self.finished = true;
            return Ok(());
        };
        let events = self.sse.push(&chunk)?;
        if self.feed(&events)? {
            // The answer ended before any SSE error in this chunk: the rest is not read.
            self.finished = true;
            return Ok(());
        }
        match self.sse.take_error() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Decodes `events` in order. Returns `true` when the decoder reached the answer's end,
    /// after its [`ChatDone`] is queued.
    fn feed(&mut self, events: &[SseEvent]) -> Result<bool, ChatError> {
        for event in events {
            if self.decoder.on_event(event, &mut self.sink)? == Flow::End {
                self.decoder.finish(&mut self.sink)?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}
