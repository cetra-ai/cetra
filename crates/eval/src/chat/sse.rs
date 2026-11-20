//! Server-sent events: the framing the three streamed APIs use. Only `event` and `data`
//! fields are read. Lines end with `\n` or `\r\n`.

use crate::error::{ChatError, EgressError};

/// One dispatched event.
pub(crate) struct SseEvent {
    /// The `event:` field, when one was sent.
    pub(crate) event: Option<String>,
    /// The `data:` lines joined with `\n`.
    pub(crate) data: String,
}

/// Splits a byte stream into events. An event larger than `max_event_bytes` fails.
pub(crate) struct SseDecoder {
    line: Vec<u8>,
    event: Option<String>,
    data: String,
    has_data: bool,
    max_event_bytes: usize,
}

impl SseDecoder {
    /// A decoder whose events are at most `max_event_bytes` long.
    pub(crate) fn new(max_event_bytes: usize) -> Self {
        Self {
            line: Vec::new(),
            event: None,
            data: String::new(),
            has_data: false,
            max_event_bytes,
        }
    }

    /// Feeds `chunk`; returns the events it completed, in order.
    ///
    /// # Errors
    ///
    /// [`ChatError::Transport`] with [`EgressError::TooLarge`] when an event passes the
    /// cap, and [`ChatError::BadStream`] for a line that is not UTF-8.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, ChatError> {
        let mut out = Vec::new();
        for &byte in chunk {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.line);
                self.line_done(&line, &mut out)?;
            } else {
                self.line.push(byte);
                if self.line.len() + self.data.len() > self.max_event_bytes {
                    return Err(self.too_large());
                }
            }
        }
        Ok(out)
    }

    /// End of stream: a last unterminated line is read, and pending data is dispatched.
    ///
    /// # Errors
    ///
    /// The errors of [`Self::push`].
    pub(crate) fn finish(&mut self) -> Result<Vec<SseEvent>, ChatError> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.line_done(&line, &mut out)?;
        }
        if self.has_data {
            out.push(self.dispatch());
        }
        Ok(out)
    }

    fn line_done(&mut self, raw: &[u8], out: &mut Vec<SseEvent>) -> Result<(), ChatError> {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        if raw.is_empty() {
            if self.has_data {
                out.push(self.dispatch());
            } else {
                self.event = None;
            }
            return Ok(());
        }
        let line = std::str::from_utf8(raw).map_err(|_| ChatError::BadStream)?;
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
                if self.data.len() > self.max_event_bytes {
                    return Err(self.too_large());
                }
            }
            "event" => self.event = Some(value.to_owned()),
            _ => {}
        }
        Ok(())
    }

    fn dispatch(&mut self) -> SseEvent {
        self.has_data = false;
        SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        }
    }

    fn too_large(&self) -> ChatError {
        ChatError::Transport(EgressError::TooLarge {
            limit: self.max_event_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SseDecoder;
    use crate::error::{ChatError, EgressError};

    const MAX: usize = 1024;

    /// The `(event, data)` pairs of `chunks`, pushed one by one, then finished.
    fn events(chunks: &[&[u8]]) -> Vec<(Option<String>, String)> {
        let mut decoder = SseDecoder::new(MAX);
        let mut out = Vec::new();
        for chunk in chunks {
            for event in decoder.push(chunk).expect("push") {
                out.push((event.event, event.data));
            }
        }
        for event in decoder.finish().expect("finish") {
            out.push((event.event, event.data));
        }
        out
    }

    fn data(text: &str) -> (Option<String>, String) {
        (None, text.to_owned())
    }

    #[test]
    fn splits_events_across_chunks() {
        let got = events(&[b"data: a\n", b"\ndata: b\r\n\r\n"]);
        assert_eq!(got, vec![data("a"), data("b")]);
    }

    #[test]
    fn joins_data_lines_and_reads_event_names() {
        let got = events(&[b"event: x\ndata: 1\ndata: 2\n\ndata: 3\n\n"]);
        assert_eq!(
            got,
            vec![(Some("x".to_owned()), "1\n2".to_owned()), data("3"),]
        );
    }

    #[test]
    fn ignores_comments_and_other_fields() {
        let got = events(&[b": ping\nid: 3\nretry: 5\ndata:no-space\n\n"]);
        assert_eq!(got, vec![data("no-space")]);
    }

    #[test]
    fn finish_dispatches_a_last_unterminated_event() {
        let got = events(&[b"data: tail"]);
        assert_eq!(got, vec![data("tail")]);
    }

    #[test]
    fn caps_an_event() {
        let mut decoder = SseDecoder::new(8);
        let err = decoder.push(b"data: 0123456789\n\n").err();
        assert_eq!(
            err,
            Some(ChatError::Transport(EgressError::TooLarge { limit: 8 }))
        );
    }

    #[test]
    fn refuses_invalid_utf8() {
        let mut decoder = SseDecoder::new(MAX);
        assert_eq!(
            decoder.push(b"data: \xff\n\n").err(),
            Some(ChatError::BadStream)
        );
    }
}
